//! Evaluation of parsed PromQL against the store, with Prometheus's
//! semantics: a 5 minute lookback for instant selectors (longer on coarse
//! tiers), staleness markers, extrapolated `rate`/`increase`, vector
//! matching. Every selector's samples are fetched once for the whole query
//! range, then each step is evaluated in memory.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::parser::{AggOp, BinOp, Expr, Grouping, Matching, Selector, Side};
use crate::obs::tsdb::{Db, Labels, MatchOp, SeriesData, is_stale};

pub const LOOKBACK_MS: i64 = 5 * 60 * 1000;
/// Most points one series of a range query may have (as Prometheus).
const MAX_POINTS: i64 = 11_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub labels: Labels,
    pub v: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub labels: Labels,
    pub points: Vec<(i64, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Scalar(f64),
    Str(String),
    Vector(Vec<Sample>),
    Matrix(Vec<Series>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Scalar(_) => "scalar",
            Value::Str(_) => "string",
            Value::Vector(_) => "vector",
            Value::Matrix(_) => "matrix",
        }
    }
}

/// Where selectors read from (the store; a fixed set in tests).
pub trait Source {
    /// (series, resolution in ms of what was read)
    fn fetch(&self, matchers: &[crate::obs::tsdb::Matcher], mint: i64, maxt: i64, max_res: i64) -> Result<(Vec<SeriesData>, i64)>;
}

impl Source for Db {
    fn fetch(&self, matchers: &[crate::obs::tsdb::Matcher], mint: i64, maxt: i64, max_res: i64) -> Result<(Vec<SeriesData>, i64)> {
        let res = self.resolution_for(mint, max_res);
        Ok((self.select(matchers, mint, maxt, max_res)?, res))
    }
}

struct Fetched {
    series: Vec<SeriesData>,
    lookback: i64,
}

pub struct Engine<'a> {
    data: HashMap<usize, Fetched>,
    _src: std::marker::PhantomData<&'a ()>,
}

fn key(s: &Selector) -> usize {
    s as *const Selector as usize
}

/// Selectors of an expression with their range (0 for instant ones).
fn selectors<'e>(e: &'e Expr, out: &mut Vec<(&'e Selector, i64)>) {
    match e {
        Expr::Selector(s) => out.push((s, 0)),
        Expr::Matrix(s, r) => out.push((s, *r)),
        Expr::Call(_, args) => args.iter().for_each(|a| selectors(a, out)),
        Expr::Aggregate { expr, param, .. } => {
            selectors(expr, out);
            if let Some(p) = param {
                selectors(p, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            selectors(lhs, out);
            selectors(rhs, out);
        }
        Expr::Neg(e) => selectors(e, out),
        Expr::Number(_) | Expr::Str(_) => {}
    }
}

impl<'a> Engine<'a> {
    /// Fetch what `expr` needs to be evaluated at every step of
    /// [start, end] (`step` 0: an instant query at `start`).
    pub fn prepare(src: &dyn Source, expr: &Expr, start: i64, end: i64, step: i64) -> Result<Self> {
        let mut sels = Vec::new();
        selectors(expr, &mut sels);
        let min_range = sels.iter().map(|(_, r)| *r).filter(|r| *r > 0).min();
        // Instant queries read the finest tier that covers them; range
        // queries may read a coarser one, as long as rate windows still
        // hold a few points.
        let max_res = if step == 0 { 1 } else { min_range.map_or(step, |r| step.min(r / 3)).max(1) };
        let mut data = HashMap::new();
        for (s, range) in sels {
            let matchers = s.all_matchers();
            let window = if range > 0 { range } else { LOOKBACK_MS };
            let mint = start - s.offset - window;
            let maxt = end - s.offset;
            let (mut series, res) = src.fetch(&matchers, mint, maxt, max_res)?;
            let lookback = LOOKBACK_MS.max(2 * res);
            if range == 0 && lookback > LOOKBACK_MS {
                let (more, _) = src.fetch(&matchers, start - s.offset - lookback, maxt, max_res)?;
                series = more;
            }
            data.insert(key(s), Fetched { series, lookback });
        }
        Ok(Engine { data, _src: std::marker::PhantomData })
    }

    pub fn eval(&self, e: &Expr, t: i64) -> Result<Value> {
        Ok(match e {
            Expr::Number(n) => Value::Scalar(*n),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Selector(s) => Value::Vector(self.instant(s, t)),
            Expr::Matrix(s, r) => Value::Matrix(self.range(s, *r, t)),
            Expr::Neg(inner) => match self.eval(inner, t)? {
                Value::Scalar(v) => Value::Scalar(-v),
                Value::Vector(v) => Value::Vector(v.into_iter().map(|s| Sample { labels: drop_name(s.labels), v: -s.v }).collect()),
                other => bail!("unary minus on a {}", other.type_name()),
            },
            Expr::Call(name, args) => self.call(name, args, t)?,
            Expr::Aggregate { op, expr, param, grouping } => self.aggregate(*op, expr, param.as_deref(), grouping, t)?,
            Expr::Binary { op, lhs, rhs, return_bool, matching } => {
                let l = self.eval(lhs, t)?;
                let r = self.eval(rhs, t)?;
                binary(*op, l, r, *return_bool, matching)?
            }
        })
    }

    fn instant(&self, s: &Selector, t: i64) -> Vec<Sample> {
        let Some(f) = self.data.get(&key(s)) else { return Vec::new() };
        let at = t - s.offset;
        f.series
            .iter()
            .filter_map(|sd| {
                let i = sd.samples.partition_point(|p| p.0 <= at);
                let (pt, v) = *sd.samples.get(i.checked_sub(1)?)?;
                (pt > at - f.lookback && !is_stale(v)).then(|| Sample { labels: sd.labels.clone(), v })
            })
            .collect()
    }

    fn range(&self, s: &Selector, range: i64, t: i64) -> Vec<Series> {
        let Some(f) = self.data.get(&key(s)) else { return Vec::new() };
        let at = t - s.offset;
        f.series
            .iter()
            .filter_map(|sd| {
                let lo = sd.samples.partition_point(|p| p.0 <= at - range);
                let hi = sd.samples.partition_point(|p| p.0 <= at);
                let points: Vec<(i64, f64)> = sd.samples[lo..hi].iter().copied().filter(|p| !is_stale(p.1)).collect();
                (!points.is_empty()).then(|| Series { labels: sd.labels.clone(), points })
            })
            .collect()
    }

    fn vector_arg(&self, e: &Expr, t: i64) -> Result<Vec<Sample>> {
        match self.eval(e, t)? {
            Value::Vector(v) => Ok(v),
            other => bail!("expected an instant vector, got a {}", other.type_name()),
        }
    }

    fn scalar_arg(&self, e: &Expr, t: i64) -> Result<f64> {
        match self.eval(e, t)? {
            Value::Scalar(v) => Ok(v),
            other => bail!("expected a scalar, got a {}", other.type_name()),
        }
    }

    fn string_arg(&self, e: &Expr, t: i64) -> Result<String> {
        match self.eval(e, t)? {
            Value::Str(s) => Ok(s),
            other => bail!("expected a string, got a {}", other.type_name()),
        }
    }

    fn matrix_arg(&self, e: &Expr, t: i64) -> Result<(Vec<Series>, i64)> {
        match e {
            Expr::Matrix(s, r) => Ok((self.range(s, *r, t), *r)),
            _ => bail!("expected a range vector (e.g. metric[5m])"),
        }
    }

    fn call(&self, name: &str, args: &[Expr], t: i64) -> Result<Value> {
        let argc = |n: usize| -> Result<()> {
            if args.len() != n {
                bail!("{name}() takes {n} argument(s), got {}", args.len());
            }
            Ok(())
        };
        let over_time = |f: &dyn Fn(&[(i64, f64)], i64, i64) -> Option<f64>| -> Result<Value> {
            let (m, range) = self.matrix_arg(&args[0], t)?;
            Ok(Value::Vector(m.into_iter().filter_map(|s| Some(Sample { v: f(&s.points, t - range, t)?, labels: drop_name(s.labels) })).collect()))
        };
        let math = |f: fn(f64) -> f64| -> Result<Value> {
            argc(1)?;
            Ok(Value::Vector(self.vector_arg(&args[0], t)?.into_iter().map(|s| Sample { v: f(s.v), labels: drop_name(s.labels) }).collect()))
        };
        Ok(match name {
            "rate" | "increase" | "delta" => {
                argc(1)?;
                let counter = name != "delta";
                let is_rate = name == "rate";
                let range_of = |e: &Expr| if let Expr::Matrix(s, r) = e { Some((s.offset, *r)) } else { None };
                let (offset, range) = range_of(&args[0]).unwrap_or((0, 0));
                let (m, _) = self.matrix_arg(&args[0], t)?;
                let end = t - offset;
                Value::Vector(
                    m.into_iter()
                        .filter_map(|s| Some(Sample { v: extrapolated(&s.points, end - range, end, counter, is_rate)?, labels: drop_name(s.labels) }))
                        .collect(),
                )
            }
            "irate" | "idelta" => {
                argc(1)?;
                over_time(&|p, _, _| {
                    let [.., (t1, v1), (t2, v2)] = p else { return None };
                    let d = if name == "irate" && v2 < v1 { *v2 } else { v2 - v1 };
                    Some(if name == "irate" { d / ((t2 - t1) as f64 / 1000.0) } else { d })
                })?
            }
            "changes" => {
                argc(1)?;
                over_time(&|p, _, _| Some(p.windows(2).filter(|w| w[0].1 != w[1].1 && !(w[0].1.is_nan() && w[1].1.is_nan())).count() as f64))?
            }
            "resets" => {
                argc(1)?;
                over_time(&|p, _, _| Some(p.windows(2).filter(|w| w[1].1 < w[0].1).count() as f64))?
            }
            "avg_over_time" => over_time(&|p, _, _| Some(p.iter().map(|x| x.1).sum::<f64>() / p.len() as f64))?,
            "sum_over_time" => over_time(&|p, _, _| Some(p.iter().map(|x| x.1).sum()))?,
            "min_over_time" => over_time(&|p, _, _| p.iter().map(|x| x.1).reduce(f64::min))?,
            "max_over_time" => over_time(&|p, _, _| p.iter().map(|x| x.1).reduce(f64::max))?,
            "count_over_time" => over_time(&|p, _, _| Some(p.len() as f64))?,
            "last_over_time" => {
                let (m, _) = self.matrix_arg(&args[0], t)?;
                Value::Vector(m.into_iter().filter_map(|s| Some(Sample { v: s.points.last()?.1, labels: s.labels })).collect())
            }
            "present_over_time" => over_time(&|_, _, _| Some(1.0))?,
            "stddev_over_time" | "stdvar_over_time" => over_time(&|p, _, _| {
                let n = p.len() as f64;
                let mean = p.iter().map(|x| x.1).sum::<f64>() / n;
                let var = p.iter().map(|x| (x.1 - mean).powi(2)).sum::<f64>() / n;
                Some(if name == "stddev_over_time" { var.sqrt() } else { var })
            })?,
            "quantile_over_time" => {
                argc(2)?;
                let q = self.scalar_arg(&args[0], t)?;
                let (m, _) = self.matrix_arg(&args[1], t)?;
                Value::Vector(
                    m.into_iter()
                        .map(|s| Sample { v: quantile(q, s.points.iter().map(|p| p.1).collect()), labels: drop_name(s.labels) })
                        .collect(),
                )
            }
            "deriv" => over_time(&|p, _, _| regression(p, t).map(|(slope, _)| slope))?,
            "predict_linear" => {
                argc(2)?;
                let ahead = self.scalar_arg(&args[1], t)?;
                let (m, _) = self.matrix_arg(&args[0], t)?;
                Value::Vector(
                    m.into_iter()
                        .filter_map(|s| {
                            let (slope, intercept) = regression(&s.points, t)?;
                            Some(Sample { v: intercept + slope * ahead, labels: drop_name(s.labels) })
                        })
                        .collect(),
                )
            }
            "absent_over_time" => {
                argc(1)?;
                let (m, _) = self.matrix_arg(&args[0], t)?;
                if m.is_empty() { Value::Vector(vec![Sample { labels: absent_labels(&args[0]), v: 1.0 }]) } else { Value::Vector(Vec::new()) }
            }
            "absent" => {
                argc(1)?;
                if self.vector_arg(&args[0], t)?.is_empty() { Value::Vector(vec![Sample { labels: absent_labels(&args[0]), v: 1.0 }]) } else { Value::Vector(Vec::new()) }
            }
            "abs" => math(f64::abs)?,
            "ceil" => math(f64::ceil)?,
            "floor" => math(f64::floor)?,
            "exp" => math(f64::exp)?,
            "ln" => math(f64::ln)?,
            "log2" => math(f64::log2)?,
            "log10" => math(f64::log10)?,
            "sqrt" => math(f64::sqrt)?,
            "sgn" => math(|v| if v > 0.0 { 1.0 } else if v < 0.0 { -1.0 } else { v })?,
            "round" => {
                let to = if args.len() > 1 { self.scalar_arg(&args[1], t)? } else { 1.0 };
                Value::Vector(self.vector_arg(&args[0], t)?.into_iter().map(|s| Sample { v: (s.v / to).round() * to, labels: drop_name(s.labels) }).collect())
            }
            "clamp" | "clamp_min" | "clamp_max" => {
                let v = self.vector_arg(&args[0], t)?;
                let (lo, hi) = match name {
                    "clamp" => {
                        argc(3)?;
                        (self.scalar_arg(&args[1], t)?, self.scalar_arg(&args[2], t)?)
                    }
                    "clamp_min" => {
                        argc(2)?;
                        (self.scalar_arg(&args[1], t)?, f64::INFINITY)
                    }
                    _ => {
                        argc(2)?;
                        (f64::NEG_INFINITY, self.scalar_arg(&args[1], t)?)
                    }
                };
                Value::Vector(v.into_iter().map(|s| Sample { v: s.v.max(lo).min(hi), labels: drop_name(s.labels) }).collect())
            }
            "time" => Value::Scalar(t as f64 / 1000.0),
            "vector" => {
                argc(1)?;
                Value::Vector(vec![Sample { labels: Vec::new(), v: self.scalar_arg(&args[0], t)? }])
            }
            "scalar" => {
                argc(1)?;
                let v = self.vector_arg(&args[0], t)?;
                Value::Scalar(if v.len() == 1 { v[0].v } else { f64::NAN })
            }
            "timestamp" => {
                argc(1)?;
                // The time of each sample is not kept past selection: the
                // evaluation time is the closest answer for live series.
                Value::Vector(self.vector_arg(&args[0], t)?.into_iter().map(|s| Sample { v: t as f64 / 1000.0, labels: drop_name(s.labels) }).collect())
            }
            "sort" | "sort_desc" => {
                argc(1)?;
                let mut v = self.vector_arg(&args[0], t)?;
                v.sort_by(|a, b| a.v.partial_cmp(&b.v).unwrap_or(std::cmp::Ordering::Equal));
                if name == "sort_desc" {
                    v.reverse();
                }
                Value::Vector(v)
            }
            "label_replace" => {
                argc(5)?;
                let v = self.vector_arg(&args[0], t)?;
                let dst = self.string_arg(&args[1], t)?;
                let repl = self.string_arg(&args[2], t)?;
                let src = self.string_arg(&args[3], t)?;
                let re = regex::Regex::new(&format!("^(?:{})$", self.string_arg(&args[4], t)?))?;
                Value::Vector(
                    v.into_iter()
                        .map(|mut s| {
                            let val = get(&s.labels, &src).to_string();
                            if let Some(caps) = re.captures(&val) {
                                let mut out = String::new();
                                caps.expand(&repl, &mut out);
                                set(&mut s.labels, &dst, &out);
                            }
                            s
                        })
                        .collect(),
                )
            }
            "label_join" => {
                if args.len() < 3 {
                    bail!("label_join() takes at least 3 arguments");
                }
                let v = self.vector_arg(&args[0], t)?;
                let dst = self.string_arg(&args[1], t)?;
                let sep = self.string_arg(&args[2], t)?;
                let srcs: Vec<String> = args[3..].iter().map(|a| self.string_arg(a, t)).collect::<Result<_>>()?;
                Value::Vector(
                    v.into_iter()
                        .map(|mut s| {
                            let joined = srcs.iter().map(|n| get(&s.labels, n)).collect::<Vec<_>>().join(&sep);
                            set(&mut s.labels, &dst, &joined);
                            s
                        })
                        .collect(),
                )
            }
            "histogram_quantile" => {
                argc(2)?;
                let q = self.scalar_arg(&args[0], t)?;
                let v = self.vector_arg(&args[1], t)?;
                let mut groups: Vec<(Labels, Vec<(f64, f64)>)> = Vec::new();
                for s in v {
                    let Ok(le) = parse_float(get(&s.labels, "le")) else { continue };
                    let key: Labels = drop_name(s.labels).into_iter().filter(|(n, _)| n != "le").collect();
                    match groups.iter_mut().find(|g| g.0 == key) {
                        Some(g) => g.1.push((le, s.v)),
                        None => groups.push((key, vec![(le, s.v)])),
                    }
                }
                Value::Vector(groups.into_iter().map(|(labels, buckets)| Sample { labels, v: bucket_quantile(q, buckets) }).collect())
            }
            "day_of_week" | "day_of_month" | "hour" | "minute" | "month" | "year" | "days_in_month" => {
                use chrono::{Datelike, TimeZone, Timelike};
                let inputs = if args.is_empty() { vec![Sample { labels: Vec::new(), v: t as f64 / 1000.0 }] } else { self.vector_arg(&args[0], t)? };
                Value::Vector(
                    inputs
                        .into_iter()
                        .filter_map(|s| {
                            let d = chrono::Utc.timestamp_opt(s.v as i64, 0).single()?;
                            let v = match name {
                                "day_of_week" => d.weekday().num_days_from_sunday() as f64,
                                "day_of_month" => d.day() as f64,
                                "hour" => d.hour() as f64,
                                "minute" => d.minute() as f64,
                                "month" => d.month() as f64,
                                "year" => d.year() as f64,
                                _ => {
                                    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
                                    let next = chrono::NaiveDate::from_ymd_opt(y, m, 1)?;
                                    next.pred_opt()?.day() as f64
                                }
                            };
                            Some(Sample { labels: drop_name(s.labels), v })
                        })
                        .collect(),
                )
            }
            other => bail!("function {other}() is not supported"),
        })
    }

    fn aggregate(&self, op: AggOp, expr: &Expr, param: Option<&Expr>, grouping: &Grouping, t: i64) -> Result<Value> {
        let input = self.vector_arg(expr, t)?;
        let group_key = |labels: &Labels| -> Labels {
            match grouping {
                Grouping::None => Vec::new(),
                Grouping::By(names) => labels.iter().filter(|(n, _)| names.contains(n)).cloned().collect(),
                Grouping::Without(names) => labels.iter().filter(|(n, _)| n != "__name__" && !names.contains(n)).cloned().collect(),
            }
        };
        let mut groups: Vec<(Labels, Vec<Sample>)> = Vec::new();
        let mut index: HashMap<Labels, usize> = HashMap::new();
        for s in input {
            let k = group_key(&s.labels);
            match index.get(&k) {
                Some(i) => groups[*i].1.push(s),
                None => {
                    index.insert(k.clone(), groups.len());
                    groups.push((k, vec![s]));
                }
            }
        }
        let mut out = Vec::new();
        match op {
            AggOp::Topk | AggOp::Bottomk => {
                let k = self.scalar_arg(param.unwrap(), t)?.max(0.0) as usize;
                for (_, mut samples) in groups {
                    samples.sort_by(|a, b| {
                        let o = b.v.partial_cmp(&a.v).unwrap_or(std::cmp::Ordering::Equal);
                        if op == AggOp::Topk { o } else { o.reverse() }
                    });
                    samples.retain(|s| !s.v.is_nan());
                    out.extend(samples.into_iter().take(k));
                }
            }
            AggOp::CountValues => {
                let label = self.string_arg(param.unwrap(), t)?;
                for (k, samples) in groups {
                    let mut counts: Vec<(String, f64)> = Vec::new();
                    for s in samples {
                        let v = format_value(s.v);
                        match counts.iter_mut().find(|c| c.0 == v) {
                            Some(c) => c.1 += 1.0,
                            None => counts.push((v, 1.0)),
                        }
                    }
                    for (v, n) in counts {
                        let mut labels = k.clone();
                        set(&mut labels, &label, &v);
                        out.push(Sample { labels, v: n });
                    }
                }
            }
            _ => {
                let q = match (op, param) {
                    (AggOp::Quantile, Some(p)) => self.scalar_arg(p, t)?,
                    _ => 0.0,
                };
                for (labels, samples) in groups {
                    let vals: Vec<f64> = samples.iter().map(|s| s.v).collect();
                    let n = vals.len() as f64;
                    let v = match op {
                        AggOp::Sum => vals.iter().sum(),
                        AggOp::Avg => vals.iter().sum::<f64>() / n,
                        AggOp::Min => vals.iter().copied().filter(|v| !v.is_nan()).reduce(f64::min).unwrap_or(f64::NAN),
                        AggOp::Max => vals.iter().copied().filter(|v| !v.is_nan()).reduce(f64::max).unwrap_or(f64::NAN),
                        AggOp::Count => n,
                        AggOp::Group => 1.0,
                        AggOp::Stddev | AggOp::Stdvar => {
                            let mean = vals.iter().sum::<f64>() / n;
                            let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
                            if op == AggOp::Stddev { var.sqrt() } else { var }
                        }
                        AggOp::Quantile => quantile(q, vals),
                        _ => unreachable!(),
                    };
                    out.push(Sample { labels, v });
                }
            }
        }
        Ok(Value::Vector(out))
    }
}

fn absent_labels(e: &Expr) -> Labels {
    let s = match e {
        Expr::Selector(s) | Expr::Matrix(s, _) => s,
        _ => return Vec::new(),
    };
    let mut labels: Labels = s.matchers.iter().filter(|m| m.op == MatchOp::Eq && m.name != "__name__").map(|m| (m.name.clone(), m.value.clone())).collect();
    labels.sort();
    labels.dedup_by(|a, b| a.0 == b.0);
    labels
}

pub fn get<'l>(labels: &'l Labels, name: &str) -> &'l str {
    labels.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap_or("")
}

fn set(labels: &mut Labels, name: &str, value: &str) {
    labels.retain(|(n, _)| n != name);
    if !value.is_empty() {
        labels.push((name.to_string(), value.to_string()));
        labels.sort_by(|a, b| a.0.cmp(&b.0));
    }
}

fn drop_name(mut labels: Labels) -> Labels {
    labels.retain(|(n, _)| n != "__name__");
    labels
}

fn parse_float(s: &str) -> Result<f64> {
    Ok(match s {
        "+Inf" | "Inf" | "inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        _ => s.parse()?,
    })
}

/// How Prometheus prints a sample value.
pub fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        format!("{v}")
    }
}

/// `rate`, `increase` and `delta` as Prometheus computes them: the change
/// over the samples in the window, extrapolated to the window's edges
/// (but not below zero for counters).
fn extrapolated(points: &[(i64, f64)], range_start: i64, range_end: i64, counter: bool, is_rate: bool) -> Option<f64> {
    if points.len() < 2 {
        return None;
    }
    let (first_t, first_v) = points[0];
    let (last_t, last_v) = *points.last().unwrap();
    let mut result = last_v - first_v;
    if counter {
        for w in points.windows(2) {
            if w[1].1 < w[0].1 {
                result += w[0].1;
            }
        }
    }
    let mut to_start = (first_t - range_start) as f64 / 1000.0;
    let to_end = (range_end - last_t) as f64 / 1000.0;
    let sampled = (last_t - first_t) as f64 / 1000.0;
    if sampled <= 0.0 {
        return None;
    }
    let avg = sampled / (points.len() - 1) as f64;
    if counter && result > 0.0 && first_v >= 0.0 {
        let to_zero = sampled * (first_v / result);
        if to_zero < to_start {
            to_start = to_zero;
        }
    }
    let threshold = avg * 1.1;
    let mut interval = sampled;
    interval += if to_start < threshold { to_start } else { avg / 2.0 };
    interval += if to_end < threshold { to_end } else { avg / 2.0 };
    let mut v = result * (interval / sampled);
    if is_rate {
        v /= (range_end - range_start) as f64 / 1000.0;
    }
    Some(v)
}

/// Least squares over the points, x relative to `t` (seconds):
/// (slope, value at t).
fn regression(points: &[(i64, f64)], t: i64) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let (mut sx, mut sy, mut sxy, mut sxx) = (0.0, 0.0, 0.0, 0.0);
    for (pt, v) in points {
        let x = (*pt - t) as f64 / 1000.0;
        sx += x;
        sy += v;
        sxy += x * v;
        sxx += x * x;
    }
    let cov = sxy - sx * sy / n;
    let var = sxx - sx * sx / n;
    if var == 0.0 {
        return None;
    }
    let slope = cov / var;
    Some((slope, sy / n - slope * sx / n))
}

fn quantile(q: f64, mut vals: Vec<f64>) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    if q < 0.0 {
        return f64::NEG_INFINITY;
    }
    if q > 1.0 {
        return f64::INFINITY;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = q * (vals.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(vals.len() - 1);
    let w = rank - lo as f64;
    vals[lo] * (1.0 - w) + vals[hi] * w
}

/// `histogram_quantile` over (upper bound, cumulative count) buckets.
fn bucket_quantile(q: f64, mut buckets: Vec<(f64, f64)>) -> f64 {
    if q < 0.0 {
        return f64::NEG_INFINITY;
    }
    if q > 1.0 {
        return f64::INFINITY;
    }
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if buckets.len() < 2 || buckets.last().unwrap().0 != f64::INFINITY {
        return f64::NAN;
    }
    // Counts must be monotonic (rates of separate buckets can wobble).
    for i in 1..buckets.len() {
        if buckets[i].1 < buckets[i - 1].1 {
            buckets[i].1 = buckets[i - 1].1;
        }
    }
    let total = buckets.last().unwrap().1;
    if total == 0.0 {
        return f64::NAN;
    }
    let rank = q * total;
    let b = buckets.iter().position(|(_, c)| *c >= rank).unwrap_or(buckets.len() - 1);
    if b == buckets.len() - 1 {
        return buckets[buckets.len() - 2].0;
    }
    if b == 0 && buckets[0].0 <= 0.0 {
        return buckets[0].0;
    }
    let (start, prev_count) = if b == 0 { (0.0, 0.0) } else { (buckets[b - 1].0, buckets[b - 1].1) };
    let (end, count) = buckets[b];
    let in_bucket = count - prev_count;
    if in_bucket == 0.0 {
        return end;
    }
    start + (end - start) * ((rank - prev_count) / in_bucket)
}

fn arith(op: BinOp, a: f64, b: f64) -> Option<f64> {
    Some(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::Mod => a % b,
        BinOp::Pow => a.powf(b),
        BinOp::Atan2 => a.atan2(b),
        BinOp::Eq => (a == b) as u8 as f64,
        BinOp::Ne => (a != b) as u8 as f64,
        BinOp::Gt => (a > b) as u8 as f64,
        BinOp::Lt => (a < b) as u8 as f64,
        BinOp::Ge => (a >= b) as u8 as f64,
        BinOp::Le => (a <= b) as u8 as f64,
        _ => return None,
    })
}

fn signature(labels: &Labels, m: &Matching) -> Labels {
    match m.on {
        Some(true) => labels.iter().filter(|(n, _)| m.labels.contains(n)).cloned().collect(),
        _ => labels.iter().filter(|(n, _)| n != "__name__" && !m.labels.contains(n)).cloned().collect(),
    }
}

fn binary(op: BinOp, l: Value, r: Value, return_bool: bool, m: &Matching) -> Result<Value> {
    let cmp = op.is_comparison();
    Ok(match (l, r) {
        (Value::Scalar(a), Value::Scalar(b)) => {
            if cmp && !return_bool {
                bail!("comparisons between scalars must use the bool modifier");
            }
            Value::Scalar(arith(op, a, b).ok_or_else(|| anyhow::anyhow!("set operators need vectors on both sides"))?)
        }
        (Value::Vector(v), Value::Scalar(s)) | (Value::Scalar(s), Value::Vector(v)) if op.is_set() => {
            let _ = (v, s);
            bail!("set operators need vectors on both sides")
        }
        (Value::Vector(v), Value::Scalar(s)) => Value::Vector(scalar_op(op, v, s, false, return_bool)),
        (Value::Scalar(s), Value::Vector(v)) => Value::Vector(scalar_op(op, v, s, true, return_bool)),
        (Value::Vector(lv), Value::Vector(rv)) => Value::Vector(vector_op(op, lv, rv, return_bool, m)?),
        (l, r) => bail!("operator not defined between a {} and a {}", l.type_name(), r.type_name()),
    })
}

fn scalar_op(op: BinOp, v: Vec<Sample>, s: f64, scalar_left: bool, return_bool: bool) -> Vec<Sample> {
    v.into_iter()
        .filter_map(|x| {
            let (a, b) = if scalar_left { (s, x.v) } else { (x.v, s) };
            let r = arith(op, a, b)?;
            if op.is_comparison() {
                if return_bool {
                    return Some(Sample { labels: drop_name(x.labels), v: r });
                }
                return (r == 1.0).then_some(x);
            }
            Some(Sample { labels: drop_name(x.labels), v: r })
        })
        .collect()
}

fn vector_op(op: BinOp, lv: Vec<Sample>, rv: Vec<Sample>, return_bool: bool, m: &Matching) -> Result<Vec<Sample>> {
    if op.is_set() {
        let rsigs: std::collections::HashSet<Labels> = rv.iter().map(|s| signature(&s.labels, m)).collect();
        return Ok(match op {
            BinOp::And => lv.into_iter().filter(|s| rsigs.contains(&signature(&s.labels, m))).collect(),
            BinOp::Unless => lv.into_iter().filter(|s| !rsigs.contains(&signature(&s.labels, m))).collect(),
            _ => {
                let lsigs: std::collections::HashSet<Labels> = lv.iter().map(|s| signature(&s.labels, m)).collect();
                let mut out = lv;
                out.extend(rv.into_iter().filter(|s| !lsigs.contains(&signature(&s.labels, m))));
                out
            }
        });
    }
    // The "one" side is indexed by signature; the "many" side looks it up.
    let (many, one, one_is_right) = match m.group {
        Some((Side::Right, _)) => (rv, lv, false),
        _ => (lv, rv, true),
    };
    let mut index: HashMap<Labels, Sample> = HashMap::new();
    for s in one {
        let sig = signature(&s.labels, m);
        if index.insert(sig, s).is_some() {
            bail!("found duplicate series for the match group on the {} side; use on()/ignoring() to match fewer labels", if one_is_right { "right" } else { "left" });
        }
    }
    let mut seen: std::collections::HashSet<Labels> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in many {
        let sig = signature(&s.labels, m);
        let Some(o) = index.get(&sig) else { continue };
        if m.group.is_none() && !seen.insert(sig.clone()) {
            bail!("multiple matches for labels: many-to-one matching must be explicit (group_left/group_right)");
        }
        let (a, b) = if one_is_right { (s.v, o.v) } else { (o.v, s.v) };
        let Some(r) = arith(op, a, b) else { continue };
        if op.is_comparison() && !return_bool && r != 1.0 {
            continue;
        }
        let value = if op.is_comparison() && !return_bool { a } else { r };
        let keep_name = op.is_comparison() && !return_bool;
        let mut labels = if keep_name { s.labels.clone() } else { drop_name(s.labels.clone()) };
        match &m.group {
            None => {
                labels = match m.on {
                    Some(true) => labels.into_iter().filter(|(n, _)| m.labels.contains(n)).collect(),
                    Some(false) => labels.into_iter().filter(|(n, _)| !m.labels.contains(n)).collect(),
                    None => labels,
                };
            }
            Some((_, extra)) => {
                for name in extra {
                    set(&mut labels, name, get(&o.labels, name));
                }
            }
        }
        out.push(Sample { labels, v: value });
    }
    Ok(out)
}

/// An instant query at `t` (ms).
pub fn instant(src: &dyn Source, query: &str, t: i64) -> Result<Value> {
    let expr = super::parser::parse(query)?;
    let engine = Engine::prepare(src, &expr, t, t, 0)?;
    engine.eval(&expr, t)
}

/// A range query: one series per label set, a point per step.
pub fn range(src: &dyn Source, query: &str, start: i64, end: i64, step: i64) -> Result<Vec<Series>> {
    if step <= 0 {
        bail!("zero or negative query resolution step");
    }
    if end < start {
        bail!("end timestamp must not be before start time");
    }
    if (end - start) / step > MAX_POINTS {
        bail!("exceeded maximum resolution of 11,000 points per timeseries. Try decreasing the query resolution (?step=XX)");
    }
    let expr = super::parser::parse(query)?;
    let engine = Engine::prepare(src, &expr, start, end, step)?;
    let mut out: Vec<Series> = Vec::new();
    let mut index: HashMap<Labels, usize> = HashMap::new();
    let mut t = start;
    while t <= end {
        let samples = match engine.eval(&expr, t)? {
            Value::Scalar(v) => vec![Sample { labels: Vec::new(), v }],
            Value::Vector(v) => v,
            other => bail!("range queries need a scalar or instant vector expression, got a {}", other.type_name()),
        };
        for s in samples {
            match index.get(&s.labels) {
                Some(i) => out[*i].points.push((t, s.v)),
                None => {
                    index.insert(s.labels.clone(), out.len());
                    out.push(Series { labels: s.labels, points: vec![(t, s.v)] });
                }
            }
        }
        t += step;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::tsdb::Matcher;

    /// Series given in full; selection by matchers only.
    struct Fixed(Vec<SeriesData>);

    impl Source for Fixed {
        fn fetch(&self, matchers: &[Matcher], mint: i64, maxt: i64, _: i64) -> Result<(Vec<SeriesData>, i64)> {
            let out = self
                .0
                .iter()
                .filter(|s| matchers.iter().all(|m| m.matches(get(&s.labels, &m.name))))
                .map(|s| SeriesData { labels: s.labels.clone(), samples: s.samples.iter().copied().filter(|p| p.0 >= mint && p.0 <= maxt).collect() })
                .collect();
            Ok((out, 10_000))
        }
    }

    fn l(pairs: &[(&str, &str)]) -> Labels {
        let mut v: Labels = pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        v.sort();
        v
    }

    fn fixture() -> Fixed {
        let every10s = |f: &dyn Fn(i64) -> f64| (0..=60).map(|i| (i * 10_000, f(i))).collect::<Vec<_>>();
        Fixed(vec![
            SeriesData { labels: l(&[("__name__", "cpu_total"), ("pod", "a"), ("ns", "x")]), samples: every10s(&|i| i as f64) },
            SeriesData { labels: l(&[("__name__", "cpu_total"), ("pod", "b"), ("ns", "x")]), samples: every10s(&|i| 2.0 * i as f64) },
            SeriesData { labels: l(&[("__name__", "cpu_total"), ("pod", "c"), ("ns", "y")]), samples: every10s(&|i| if i < 30 { i as f64 } else { (i - 30) as f64 }) },
            SeriesData { labels: l(&[("__name__", "mem"), ("pod", "a"), ("ns", "x")]), samples: every10s(&|_| 100.0) },
            SeriesData { labels: l(&[("__name__", "mem"), ("pod", "b"), ("ns", "x")]), samples: every10s(&|_| 300.0) },
            SeriesData { labels: l(&[("__name__", "info"), ("pod", "a"), ("node", "n1")]), samples: every10s(&|_| 1.0) },
            SeriesData { labels: l(&[("__name__", "info"), ("pod", "b"), ("node", "n2")]), samples: every10s(&|_| 1.0) },
            SeriesData { labels: l(&[("__name__", "gone"), ("pod", "z")]), samples: vec![(0, 1.0), (100_000, 1.0), (110_000, crate::obs::tsdb::stale())] },
        ])
    }

    fn vector(q: &str, t: i64) -> Vec<(Labels, f64)> {
        match instant(&fixture(), q, t).unwrap_or_else(|e| panic!("{q}: {e}")) {
            Value::Vector(v) => {
                let mut v: Vec<_> = v.into_iter().map(|s| (s.labels, s.v)).collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            }
            other => panic!("{q}: {other:?}"),
        }
    }

    fn scalar_of(q: &str, t: i64) -> f64 {
        match instant(&fixture(), q, t).unwrap() {
            Value::Scalar(v) => v,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn selectors_lookback_and_staleness() {
        assert_eq!(vector(r#"mem{pod="a"}"#, 600_000), vec![(l(&[("__name__", "mem"), ("pod", "a"), ("ns", "x")]), 100.0)]);
        // 5 minutes after the last sample, nothing.
        assert!(vector("mem", 600_000 + 300_000).is_empty());
        assert_eq!(vector("mem", 600_000 + 299_000).len(), 2);
        // A stale marker ends a series at once.
        assert_eq!(vector("gone", 105_000).len(), 1);
        assert!(vector("gone", 115_000).is_empty());
        assert_eq!(vector(r#"cpu_total{pod=~"a|b"} offset 100s"#, 600_000)[0].1, 50.0);
    }

    #[test]
    fn rates() {
        // 1 per 10 s = 0.1/s exactly (samples cover the window).
        let r = vector(r#"rate(cpu_total{pod="a"}[5m])"#, 600_000);
        assert!((r[0].1 - 0.1).abs() < 1e-9, "{r:?}");
        assert_eq!(r[0].0, l(&[("pod", "a"), ("ns", "x")]));
        let inc = vector(r#"increase(cpu_total{pod="b"}[1m])"#, 600_000);
        assert!((inc[0].1 - 12.0).abs() < 1e-9, "{inc:?}");
        // A counter reset is not a drop.
        let c = vector(r#"rate(cpu_total{pod="c"}[10m])"#, 600_000);
        assert!((c[0].1 - 0.1).abs() < 0.01, "{c:?}");
        let ir = vector(r#"irate(cpu_total{pod="b"}[1m])"#, 600_000);
        assert!((ir[0].1 - 0.2).abs() < 1e-9);
        assert_eq!(vector(r#"resets(cpu_total{pod="c"}[10m])"#, 600_000)[0].1, 1.0);
        assert_eq!(vector(r#"max_over_time(cpu_total{pod="a"}[1m])"#, 600_000)[0].1, 60.0);
        assert_eq!(vector(r#"count_over_time(cpu_total{pod="a"}[1m])"#, 600_000)[0].1, 6.0);
        let d = vector(r#"deriv(cpu_total{pod="a"}[2m])"#, 600_000);
        assert!((d[0].1 - 0.1).abs() < 1e-9);
        let p = vector(r#"predict_linear(cpu_total{pod="a"}[2m], 100)"#, 600_000);
        assert!((p[0].1 - 70.0).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn aggregations() {
        let s = vector("sum by (ns) (mem)", 100_000);
        assert_eq!(s, vec![(l(&[("ns", "x")]), 400.0)]);
        assert_eq!(vector("sum(mem)", 100_000), vec![(Vec::new(), 400.0)]);
        assert_eq!(vector("avg without (pod) (mem)", 100_000), vec![(l(&[("ns", "x")]), 200.0)]);
        assert_eq!(vector("count(cpu_total)", 100_000)[0].1, 3.0);
        let top = vector("topk(1, cpu_total)", 100_000);
        assert_eq!(get(&top[0].0, "pod"), "b");
        let bottom = vector("bottomk(2, mem)", 100_000);
        assert_eq!(bottom.len(), 2);
        assert_eq!(vector("max(mem) by (ns)", 100_000)[0].1, 300.0);
        assert_eq!(vector("quantile(0.5, mem)", 100_000)[0].1, 200.0);
        assert_eq!(vector(r#"count_values("v", mem)"#, 100_000).len(), 2);
    }

    #[test]
    fn binary_operators_and_matching() {
        assert_eq!(scalar_of("1 + 2 * 3 ^ 2", 0), 19.0);
        assert_eq!(scalar_of("2 > bool 1", 0), 1.0);
        assert!(instant(&fixture(), "2 > 1", 0).is_err());
        assert_eq!(vector("mem / 100", 100_000).iter().map(|s| s.1).collect::<Vec<_>>(), vec![1.0, 3.0]);
        let filtered = vector("mem > 200", 100_000);
        assert_eq!(filtered.len(), 1);
        assert_eq!(get(&filtered[0].0, "__name__"), "mem", "a filter keeps the name");
        assert_eq!(vector("mem > bool 200", 100_000).iter().map(|s| s.1).collect::<Vec<_>>(), vec![0.0, 1.0]);
        // One-to-one on identical label sets (name aside).
        let ratio = vector(r#"cpu_total{ns="x"} / mem"#, 100_000);
        assert_eq!(ratio.len(), 2);
        // group_left copies the node label from info.
        let joined = vector("mem * on(pod) group_left(node) info", 100_000);
        assert_eq!(joined.len(), 2);
        assert_eq!(get(&joined[0].0, "node"), "n1");
        assert_eq!(get(&joined[0].0, "ns"), "x");
        // Many-to-one without group_left is an error.
        assert!(instant(&fixture(), "cpu_total * on(ns) mem", 100_000).is_err());
        assert_eq!(vector(r#"mem and on(pod) cpu_total{pod="a"}"#, 100_000).len(), 1);
        assert_eq!(vector(r#"mem unless on(pod) cpu_total{pod="a"}"#, 100_000).len(), 1);
        assert_eq!(vector(r#"mem or cpu_total"#, 100_000).len(), 3, "same labels but the name: lhs wins");
        assert_eq!(vector("-mem", 100_000)[0].1, -100.0);
    }

    #[test]
    fn functions() {
        assert_eq!(vector(r#"absent(nothing{job="x"})"#, 0), vec![(l(&[("job", "x")]), 1.0)]);
        assert!(vector("absent(mem)", 100_000).is_empty());
        assert_eq!(vector("vector(3)", 0), vec![(Vec::new(), 3.0)]);
        assert_eq!(scalar_of("scalar(sum(mem))", 100_000), 400.0);
        assert_eq!(scalar_of("time()", 5_000), 5.0);
        let r = vector(r#"label_replace(mem, "short", "p-$1", "pod", "(.)")"#, 100_000);
        assert_eq!(get(&r[0].0, "short"), "p-a");
        assert_eq!(vector("clamp_max(mem, 150)", 100_000).iter().map(|s| s.1).collect::<Vec<_>>(), vec![100.0, 150.0]);
        assert_eq!(vector("sort_desc(mem)", 100_000).len(), 2);
    }

    #[test]
    fn histogram_quantiles() {
        let b = |le: f64, c: f64| (le, c);
        assert_eq!(bucket_quantile(0.5, vec![b(1.0, 0.0), b(2.0, 10.0), b(f64::INFINITY, 10.0)]), 1.5);
        assert_eq!(bucket_quantile(0.99, vec![b(1.0, 5.0), b(f64::INFINITY, 10.0)]), 1.0);
        assert!(bucket_quantile(0.5, vec![b(1.0, 0.0), b(f64::INFINITY, 0.0)]).is_nan());
    }

    #[test]
    fn range_query_steps() {
        let r = range(&fixture(), r#"mem{pod="a"} * 2"#, 0, 120_000, 30_000).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].points, vec![(0, 200.0), (30_000, 200.0), (60_000, 200.0), (90_000, 200.0), (120_000, 200.0)]);
        assert!(range(&fixture(), "mem", 0, 1_000_000_000, 1).is_err());
        let s = range(&fixture(), "1+1", 0, 20_000, 10_000).unwrap();
        assert_eq!(s[0].points.len(), 3);
    }
}
