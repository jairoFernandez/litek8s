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
    // The data is the strategic-merge patch the controllers record (clients
    // compare it byte for byte with their own): the template, replaced
    // whole, with keys in order and the creationTimestamp Go always writes.
    let mut t = template.clone();
    if !t.get("metadata").is_some_and(Value::is_object) {
        t["metadata"] = json!({});
    }
    if t["metadata"].get("creationTimestamp").is_none() {
        t["metadata"]["creationTimestamp"] = Value::Null;
    }
    t["$patch"] = json!("replace");
    let data = json!({"spec": {"template": t}});
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
    let mut t = api.get_object("apps", "controllerrevisions", ns, name)?.pointer("/data/spec/template").cloned()?;
    if let Some(m) = t.as_object_mut() {
        m.remove("$patch");
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::controllers::util::s;

    fn template(image: &str) -> Value {
        json!({"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "c", "image": image}]}})
    }

    fn setup(limit: Option<i64>) -> (ApiState, Value) {
        let api = ApiState::open(None, "127.0.0.1:0".into(), false).unwrap();
        let mut spec = json!({"selector": {"matchLabels": {"app": "web"}}, "template": template("v1")});
        if let Some(limit) = limit {
            spec["revisionHistoryLimit"] = json!(limit);
        }
        let sts = json!({"apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": {"name": "web"}, "spec": spec});
        let owner = api.create_object("apps", "statefulsets", Some("default"), sts).unwrap();
        (api, owner)
    }

    fn sync_image(api: &ApiState, owner: &Value, image: &str, in_use: &[String]) -> String {
        let t = template(image);
        sync(api, owner, &t, &util::hash_of(&t), in_use).unwrap().name
    }

    /// (name, revision) of every revision, oldest first.
    fn revisions(api: &ApiState, owner: &Value) -> Vec<(String, i64)> {
        list(api, "default", owner).iter().map(|r| (util::name(r), r["revision"].as_i64().unwrap())).collect()
    }

    #[test]
    fn records_one_revision_per_template() {
        let (api, owner) = setup(None);
        let v1 = sync_image(&api, &owner, "v1", &[]);
        let t = template("v1");
        assert_eq!(v1, format!("web-{}", util::hash_of(&t)));
        assert_eq!(sync_image(&api, &owner, "v1", &[]), v1);
        assert_eq!(revisions(&api, &owner), [(v1.clone(), 1)]);

        let cr = api.get_object("apps", "controllerrevisions", "default", &v1).unwrap();
        assert_eq!(s(&cr, "/metadata/labels/app"), "web");
        assert_eq!(s(&cr, "/metadata/labels/controller-revision-hash"), util::hash_of(&t));
        assert_eq!(s(&cr, "/metadata/ownerReferences/0/uid"), util::uid(&owner));
        // The data clients compare: the template replaced whole.
        assert_eq!(cr.pointer("/data/spec/template/$patch"), Some(&json!("replace")));
        assert_eq!(cr.pointer("/data/spec/template/metadata/creationTimestamp"), Some(&Value::Null));

        let v2 = sync_image(&api, &owner, "v2", &[]);
        assert_eq!(revisions(&api, &owner), [(v1, 1), (v2, 2)]);
    }

    #[test]
    fn rollback_renumbers_the_old_revision() {
        let (api, owner) = setup(None);
        let v1 = sync_image(&api, &owner, "v1", &[]);
        let v2 = sync_image(&api, &owner, "v2", &[]);
        assert_eq!(sync_image(&api, &owner, "v1", &[]), v1);
        assert_eq!(revisions(&api, &owner), [(v2, 2), (v1, 3)]);
    }

    #[test]
    fn trims_history_but_keeps_revisions_in_use() {
        let (api, owner) = setup(Some(1));
        let v1 = sync_image(&api, &owner, "v1", &[]);
        let v2 = sync_image(&api, &owner, "v2", &[]);
        let v3 = sync_image(&api, &owner, "v3", &[]);
        // One old revision besides the current one.
        assert_eq!(revisions(&api, &owner), [(v2.clone(), 2), (v3.clone(), 3)]);
        assert!(api.get_object("apps", "controllerrevisions", "default", &v1).is_none());

        // Pods still on v2 keep it, beyond the limit.
        let v4 = sync_image(&api, &owner, "v4", &[v2.clone()]);
        assert_eq!(revisions(&api, &owner), [(v2.clone(), 2), (v3, 3), (v4.clone(), 4)]);
        let v5 = sync_image(&api, &owner, "v5", &[v2.clone()]);
        assert_eq!(revisions(&api, &owner), [(v2, 2), (v4, 4), (v5, 5)]);
    }

    #[test]
    fn limit_zero_keeps_only_the_current_revision() {
        let (api, owner) = setup(Some(0));
        sync_image(&api, &owner, "v1", &[]);
        let v2 = sync_image(&api, &owner, "v2", &[]);
        assert_eq!(revisions(&api, &owner), [(v2, 2)]);
    }

    #[test]
    fn ignores_revisions_of_other_owners() {
        let (api, owner) = setup(None);
        let other = api
            .create_object("apps", "statefulsets", Some("default"),
                json!({"apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": {"name": "db"}, "spec": {"selector": {"matchLabels": {"app": "db"}}, "template": template("v1")}}))
            .unwrap();
        sync_image(&api, &other, "v1", &[]);
        sync_image(&api, &other, "v2", &[]);
        let mine = sync_image(&api, &owner, "v1", &[]);
        assert_eq!(revisions(&api, &owner), [(mine, 1)]);
        assert_eq!(revisions(&api, &other).len(), 2);
    }

    #[test]
    fn template_of_gives_back_the_recorded_template() {
        let (api, owner) = setup(None);
        let name = sync_image(&api, &owner, "v1", &[]);
        let mut expected = template("v1");
        expected["metadata"]["creationTimestamp"] = Value::Null;
        assert_eq!(template_of(&api, "default", &name), Some(expected));
        assert_eq!(template_of(&api, "default", "missing"), None);
    }
}
