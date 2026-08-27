//! Inline swarm subtree filtering, split out of `tui_state.rs` to keep that
//! file within the code-size budget.

/// Restrict swarm members to the descendants `self_id` actually spawned: every
/// member whose `report_back_to_session_id` chain reaches `self_id`, *excluding*
/// `self_id` itself.
///
/// This keeps the inline swarm strip scoped to the agents a session manages,
/// without listing the viewing session as one of "its" agents and without
/// showing unrelated members that merely share the swarm (e.g. other sessions in
/// the same repository).
///
/// Returns empty when the session has not spawned anyone, which the caller uses
/// to hide the strip entirely.
pub(crate) fn filter_inline_swarm_subtree(
    members: &[crate::protocol::SwarmMemberStatus],
    self_id: &str,
) -> Vec<crate::protocol::SwarmMemberStatus> {
    use std::collections::{HashMap, HashSet};

    // Build the parent -> children index once, then walk outward from this
    // session. The previous implementation rebuilt a cycle-detection HashSet
    // while walking the parent chain for every member, on every frame. Large,
    // long-lived swarms made that input render path needlessly expensive.
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for member in members {
        if let Some(parent) = member.report_back_to_session_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(member.session_id.as_str());
        }
    }

    let mut descendants: HashSet<&str> = HashSet::new();
    let mut pending = vec![self_id];
    while let Some(parent) = pending.pop() {
        let Some(children) = children_by_parent.get(parent) else {
            continue;
        };
        for &child in children {
            // This both excludes cycles and ensures each subtree node is
            // expanded at most once.
            if child != self_id && descendants.insert(child) {
                pending.push(child);
            }
        }
    }

    members
        .iter()
        .filter(|m| descendants.contains(m.session_id.as_str()))
        .cloned()
        .collect()
}
