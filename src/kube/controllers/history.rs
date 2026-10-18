//! ControllerRevisions: the template history StatefulSets and DaemonSets
//! keep, so pods of an older revision can be recreated as they were and a
//! rollback finds its revision again.

use serde_json::{Value, json};

use super::util;
use crate::kube::server::{ApiError, ApiState};

pub(crate) struct Revision {
    pub name: String,
}

/// The revisions `owner` controls, oldest first.
pub(crate) fn list(api: &ApiState, ns: &str, owner: &Value) -> Vec<Value> {
    let mut revs = util::owned(api, "apps", "controllerrevisions", ns, owner);
    revs.sort_by_key(|r| r.get("revision").and_then(Value::as_i64).unwrap_or(0));
    revs
}

/// The revision holding `template` (created if new, renumbered as the
/// newest when an old one becomes current again), with history trimmed to
/// `revisionHistoryLimit` (keeping `in_use`).
pub(crate) fn sync(api: &ApiState, owner: &Value, template: &Value, hash: &str, in_use: &[String]) -> Result<Revision, ApiError> {
    let (ns, owner_name) = util::ns_name(owner);
    let data = json!({"spec": {"template": template}});
    let revs = list(api, &ns, owner);
    let max = revs.iter().filter_map(|r| r.get("revision").and_then(Value::as_i64)).max().unwrap_or(0);
    let same: Vec<&Value> = revs.iter().filter(|r| r.get("data") == Some(&data)).collect();
    let current = match same.iter().max_by_key(|r| r.get("revision").and_then(Value::as_i64).unwrap_or(0)) {
        Some(r) => {
            let rev = r.get("revision").and_then(Value::as_i64).unwrap_or(0);
            if rev < max {
                let mut bumped = (*r).clone();
                bumped["revision"] = json!(max + 1);
                api.update_object("apps", "controllerrevisions", bumped)?
            } else {
                (*r).clone()
            }
        }
        None => {
            let mut labels = template.pointer("/metadata/labels").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            labels["controller-revision-hash"] = json!(hash);
            let cr = json!({
                "apiVersion": "apps/v1",
                "kind": "ControllerRevision",
                "metadata": {"name": format!("{owner_name}-{hash}"), "labels": labels, "ownerReferences": [util::owner_ref(owner)]},
                "data": data,
                "revision": max + 1,
            });
            match api.create_object("apps", "controllerrevisions", Some(&ns), cr) {
                Ok(c) => c,
                Err(e) if e.is_already_exists() => {
                    // A revision of that name that is not ours (or not yet
                    // seen as ours): use it as it is.
                    api.get_object("apps", "controllerrevisions", &ns, &format!("{owner_name}-{hash}")).ok_or(e)?
                }
                Err(e) => return Err(e),
            }
        }
    };
    let limit = util::i(owner, "/spec/revisionHistoryLimit").unwrap_or(10).max(0) as usize;
    let current_name = util::name(&current);
    let old: Vec<Value> = list(api, &ns, owner)
        .into_iter()
        .filter(|r| util::name(r) != current_name && !in_use.contains(&util::name(r)))
        .collect();
    if old.len() > limit {
        for r in &old[..old.len() - limit] {
            let _ = api.delete_named("apps", "controllerrevisions", &ns, &util::name(r));
        }
    }
    Ok(Revision { name: current_name })
}

/// The template a named revision recorded.
pub(crate) fn template_of(api: &ApiState, ns: &str, name: &str) -> Option<Value> {
    api.get_object("apps", "controllerrevisions", ns, name)?.pointer("/data/spec/template").cloned()
}
