//! The macOS VM's memory ceiling: how much of its memory the guest may use
//! right now, moved with the virtio balloon.
//!
//! Virtualization.framework never gives memory back to macOS: every guest
//! page the VM has touched once stays charged to its process until the VM
//! stops. Its balloon does not change that (it has no free page reporting,
//! and pages handed to it stay charged), but it does keep the guest off
//! pages it has never touched. So the VM starts small and grows when it
//! needs to: page cache (an image pull, a build reading files) then reuses
//! the memory the guest already has instead of spreading over all of it,
//! and macOS charges the VM about what its workloads needed at their peak,
//! not its whole size. The balloon driver does not touch the pages it
//! takes when the guest boots with `init_on_alloc=0` (else it zeroes them,
//! and the VM's footprint grows by everything it takes).
//!
//! The guest's init reports its memory on the console (`LITEK8S_MEM ...`,
//! see [`Report`]) every few seconds when it changed, and at once when a
//! PSI trigger fires; the dev VM process ([`super::vm`]) feeds the reports
//! to a [`Controller`] and sets the balloon's target. The ceiling grows at
//! once on pressure (memory stalls, little memory available) and shrinks
//! slowly once the guest has been calm for a while, which also trims page
//! cache nobody reads (the balloon takes the memory the guest frees by
//! reclaiming its oldest cache). Should a workload outrun the controller,
//! the driver deflates on its own before the OOM killer would kill
//! anything (VIRTIO_BALLOON_F_DEFLATE_ON_OOM, which the framework offers).

use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;

/// One report of the guest's memory.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Report {
    /// MemAvailable: free memory plus what the guest could reclaim.
    pub available: u64,
    /// MemFree.
    pub free: u64,
    /// Memory held by the balloon (not the guest's to use).
    pub balloon: u64,
    /// PSI memory `some` avg10, in percent.
    pub stall: f32,
    /// A PSI trigger fired since the last report.
    pub pressure: bool,
}

impl Report {
    /// The console line's payload: `available=<bytes> free=<bytes> balloon=<bytes> stall=<pct> pressure=<0|1>`.
    pub fn to_line(&self) -> String {
        format!("available={} free={} balloon={} stall={:.2} pressure={}", self.available, self.free, self.balloon, self.stall, u8::from(self.pressure))
    }

    pub fn parse(line: &str) -> Option<Report> {
        let mut r = Report::default();
        let mut seen = 0;
        for word in line.split_whitespace() {
            let Some((k, v)) = word.split_once('=') else { continue };
            match k {
                "available" => r.available = v.parse().ok()?,
                "free" => r.free = v.parse().ok()?,
                "balloon" => r.balloon = v.parse().ok()?,
                "stall" => r.stall = v.parse().ok()?,
                "pressure" => r.pressure = v == "1",
                _ => continue,
            }
            seen += 1;
        }
        (seen >= 3).then_some(r)
    }

    /// Whether `self` is worth sending after `last` (the controller only
    /// acts on changes of tens of MiB).
    pub fn differs(&self, last: &Report) -> bool {
        let far = |a: u64, b: u64| a.abs_diff(b) >= 32 * MIB;
        self.pressure || far(self.available, last.available) || far(self.free, last.free) || self.balloon != last.balloon || (self.stall > 0.0) != (last.stall > 0.0)
    }
}

/// Pages in the balloon, from `/proc/vmstat`'s cumulative counters.
pub fn parse_vmstat_balloon(text: &str) -> u64 {
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse::<u64>().ok()).unwrap_or(0);
    field("balloon_inflate").saturating_sub(field("balloon_deflate"))
}

/// `some avg10` of `/proc/pressure/memory`, in percent.
pub fn parse_psi_some_avg10(text: &str) -> Option<f32> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace().find_map(|w| w.strip_prefix("avg10="))?.parse().ok()
}

/// The guest's memory now (Linux; the guest's init calls it).
#[cfg(target_os = "linux")]
pub fn read(pressure: bool) -> Option<Report> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |name: &str| meminfo.lines().find_map(|l| l.strip_prefix(name)?.split_whitespace().next()?.parse::<u64>().ok()).map(|v| v * 1024);
    let page = 4096; // the guest kernel's page size (arm64, 4K pages)
    let balloon = std::fs::read_to_string("/proc/vmstat").map(|t| parse_vmstat_balloon(&t) * page).unwrap_or(0);
    let stall = std::fs::read_to_string("/proc/pressure/memory").ok().and_then(|t| parse_psi_some_avg10(&t)).unwrap_or(0.0);
    Some(Report { available: kb("MemAvailable:")?, free: kb("MemFree:")?, balloon, stall, pressure })
}

/// When the ceiling may move.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Never below this (nor above the VM's size).
    pub floor: u64,
    /// Where the VM starts.
    pub initial: u64,
    /// Grow when MemAvailable drops under this...
    pub reserve: u64,
    /// ...or memory stalls reach this (PSI some avg10, percent).
    pub stall_grow: f32,
    /// Shrink only after this long without pressure or growth...
    pub calm: Duration,
    /// ...at most once per this...
    pub shrink_every: Duration,
    /// ...and never by less than this.
    pub min_step: u64,
}

impl Policy {
    pub fn for_vm(vm: u64) -> Policy {
        let gib = 1024 * MIB;
        Policy {
            floor: vm.min(gib),
            initial: vm.min(gib.max(vm / 4)),
            reserve: (vm / 16).max(256 * MIB),
            stall_grow: 1.0,
            calm: Duration::from_secs(120),
            shrink_every: Duration::from_secs(60),
            min_step: 128 * MIB,
        }
    }
}

/// Why the ceiling moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Memory stalls or a PSI trigger.
    Pressure,
    /// Little memory available.
    Low,
    /// Calm for a while with plenty available.
    Idle,
}

/// The ceiling and what moved it last.
#[derive(Debug)]
pub struct Controller {
    vm: u64,
    policy: Policy,
    limit: u64,
    last_grow: Option<Instant>,
    last_shrink: Option<Instant>,
    calm_since: Option<Instant>,
    /// The balloon in the last report.
    balloon: Option<u64>,
}

/// Round down to whole MiB (what the framework takes).
fn mib_floor(b: u64) -> u64 {
    b / MIB * MIB
}

impl Controller {
    pub fn new(vm: u64, policy: Policy, now: Instant) -> Controller {
        Controller { vm, policy, limit: mib_floor(policy.initial.clamp(policy.floor.min(vm), vm)), last_grow: None, last_shrink: None, calm_since: Some(now), balloon: None }
    }

    /// What the guest may use now: the balloon's target.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn vm(&self) -> u64 {
        self.vm
    }

    /// A new ceiling, if this report calls for one.
    pub fn on_report(&mut self, r: &Report, now: Instant) -> Option<(u64, Why)> {
        let p = self.policy;
        // What the guest has now (the balloon may lag behind its target,
        // or have deflated by itself on OOM).
        let actual = self.vm.saturating_sub(r.balloon).max(self.vm.min(p.floor));
        // Stalls are the ceiling's doing only when the guest is short of
        // free memory; with plenty free they come from elsewhere (a pod at
        // its own memory limit stalls too, and PSI counts it system-wide),
        // and more memory would not help.
        let binding = r.free < p.reserve;
        let pressure = binding && (r.pressure || r.stall >= p.stall_grow);
        let low = r.available < p.reserve;
        if pressure || low {
            self.calm_since = None;
            if self.limit >= self.vm {
                return None;
            }
            // Severe (little left, stalls of 10 %): everything at once;
            // else half again as much, and at least 512 MiB.
            let severe = r.available < p.reserve / 2 || (pressure && r.stall >= 10.0);
            let base = actual.max(self.limit);
            let grown = if severe { self.vm } else { base + (base / 2).max(512 * MIB) };
            self.limit = mib_floor(grown.min(self.vm));
            self.last_grow = Some(now);
            return Some((self.limit, if pressure { Why::Pressure } else { Why::Low }));
        }
        if binding && r.stall > 0.0 {
            self.calm_since = None;
            return None;
        }
        let calm_since = *self.calm_since.get_or_insert(now);
        let calm = now.duration_since(calm_since) >= p.calm && self.last_grow.is_none_or(|t| now.duration_since(t) >= p.calm);
        let due = self.last_shrink.is_none_or(|t| now.duration_since(t) >= p.shrink_every);
        if !calm || !due {
            return None;
        }
        // Keep twice the reserve available; give up half the excess at a
        // time, so hot cache has a chance to show up as stalls first.
        let keep = 2 * p.reserve;
        if r.available <= keep {
            return None;
        }
        let excess = r.available - keep;
        let target = mib_floor(actual.min(self.limit).saturating_sub(excess / 2).max(p.floor));
        if target + p.min_step > self.limit {
            return None;
        }
        self.limit = target;
        self.last_shrink = Some(now);
        Some((target, Why::Idle))
    }

    /// The balloon deflated by itself (the guest ran out of memory before
    /// the ceiling grew): follow what the guest has, so the next decision
    /// starts from there. A balloon still on its way to a lower ceiling
    /// (it takes a few seconds per GiB) is not that.
    pub fn observe(&mut self, r: &Report) {
        let shrank = self.balloon.is_some_and(|b| r.balloon < b);
        self.balloon = Some(r.balloon);
        let actual = self.vm.saturating_sub(r.balloon);
        if shrank && actual > self.limit + 64 * MIB {
            self.limit = mib_floor(actual.min(self.vm));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn report(available: u64, balloon: u64) -> Report {
        Report { available, free: available / 2, balloon, stall: 0.0, pressure: false }
    }

    #[test]
    fn parses_what_the_guest_reports() {
        let r = Report { available: 3 * GIB, free: GIB, balloon: 2 * GIB, stall: 1.5, pressure: true };
        assert_eq!(Report::parse(&r.to_line()), Some(r));
        assert_eq!(Report::parse("available=1 free=2 balloon=3 extra=x").map(|r| r.balloon), Some(3));
        assert_eq!(Report::parse("ip=1.2.3.4"), None);
        assert_eq!(parse_vmstat_balloon("pgfault 9\nballoon_inflate 786432\nballoon_deflate 118784\nballoon_migrate 0\n"), 667648);
        assert_eq!(parse_vmstat_balloon("pgfault 9\n"), 0);
        let psi = "some avg10=7.60 avg60=1.37 avg300=0.28 total=1575774\nfull avg10=7.42 avg60=1.34 avg300=0.28 total=1549459\n";
        assert_eq!(parse_psi_some_avg10(psi), Some(7.6));
        assert!(report(GIB, 0).differs(&report(GIB - 40 * MIB, 0)));
        assert!(!report(GIB, 0).differs(&report(GIB - 10 * MIB, 0)));
    }

    #[test]
    fn starts_small_and_grows_at_once_under_pressure() {
        let t = Instant::now();
        let vm = 4 * GIB;
        let mut c = Controller::new(vm, Policy::for_vm(vm), t);
        assert_eq!(c.limit(), GIB);
        // Idle at 1 GiB: nothing to do yet.
        assert_eq!(c.on_report(&report(700 * MIB, 3 * GIB), t), None);
        // Available under the reserve: half again.
        assert_eq!(c.on_report(&report(200 * MIB, 3 * GIB), t), Some((1536 * MIB, Why::Low)));
        // A PSI trigger: half again of that.
        let stalled = Report { pressure: true, ..report(400 * MIB, 2560 * MIB) };
        assert_eq!(c.on_report(&stalled, t), Some((2304 * MIB, Why::Pressure)));
        // Stalls with plenty free are not the ceiling's (a pod at its own
        // limit): nothing to do.
        let elsewhere = Report { stall: 30.0, pressure: true, free: 900 * MIB, ..report(1200 * MIB, 1792 * MIB) };
        assert_eq!(c.on_report(&elsewhere, t), None);
        // Severe: all of it.
        let severe = Report { stall: 12.0, ..report(300 * MIB, 1792 * MIB) };
        assert_eq!(c.on_report(&severe, t), Some((vm, Why::Pressure)));
        assert_eq!(c.on_report(&severe, t), None);
    }

    #[test]
    fn shrinks_slowly_once_calm() {
        let t = Instant::now();
        let vm = 4 * GIB;
        let p = Policy::for_vm(vm);
        let mut c = Controller::new(vm, p, t);
        c.on_report(&Report { stall: 20.0, ..report(100 * MIB, 3 * GIB) }, t);
        assert_eq!(c.limit(), vm);
        // Plenty available (cache from an image pull), but just grew.
        let idle = report(3200 * MIB, 0);
        assert_eq!(c.on_report(&idle, t + Duration::from_secs(60)), None);
        // Calm for two minutes after the growth: half the excess over
        // twice the reserve goes.
        let t1 = t + Duration::from_secs(181);
        assert_eq!(c.on_report(&idle, t1), Some((2752 * MIB, Why::Idle)));
        // Not again within a minute.
        assert_eq!(c.on_report(&report(1800 * MIB, 1536 * MIB), t1 + Duration::from_secs(30)), None);
        let t2 = t1 + Duration::from_secs(61);
        assert_eq!(c.on_report(&report(1800 * MIB, 1536 * MIB), t2), Some((1916 * MIB, Why::Idle)));
        // Stalls reset the calm (when free memory is short).
        assert_eq!(c.on_report(&Report { stall: 0.3, free: 100 * MIB, ..report(1300 * MIB, 2 * GIB) }, t2 + Duration::from_secs(70)), None);
        assert_eq!(c.on_report(&report(1300 * MIB, 2 * GIB), t2 + Duration::from_secs(80)), None);
        // Never under the floor; small steps are not worth it.
        let t3 = t2 + Duration::from_secs(400);
        assert_eq!(c.on_report(&report(1300 * MIB, 2 * GIB), t3), Some((1522 * MIB, Why::Idle)));
        assert_eq!(c.on_report(&report(600 * MIB, 2560 * MIB), t3 + Duration::from_secs(61)), None);
        let mut small = Controller::new(GIB, Policy::for_vm(GIB), t);
        assert_eq!(small.limit(), GIB);
        assert_eq!(small.on_report(&report(900 * MIB, 0), t + Duration::from_secs(500)), None);
    }

    #[test]
    fn follows_a_balloon_that_deflated_by_itself() {
        let t = Instant::now();
        let vm = 4 * GIB;
        let mut c = Controller::new(vm, Policy::for_vm(vm), t);
        // Inflating towards the 1 GiB ceiling: not a deflation.
        c.observe(&report(500 * MIB, GIB));
        c.observe(&report(500 * MIB, 2 * GIB));
        assert_eq!(c.limit(), GIB);
        c.observe(&report(500 * MIB, 3 * GIB));
        assert_eq!(c.limit(), GIB);
        // Deflated by 1 GiB on OOM.
        c.observe(&report(100 * MIB, 2 * GIB));
        assert_eq!(c.limit(), 2 * GIB);
    }
}
