use std::borrow::Borrow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
///
/// Every allocated id carries a process epoch (`pid` plus the nanoseconds at
/// this process's first allocation), so ids from two different process epochs
/// are always separated even when their `micros` and `counter` parts coincide.
/// All four parts are written as separate `_`-delimited fields, so two
/// different `(pid, first_alloc_nanos, micros, counter)` tuples always render
/// different strings. Distinct epochs still rely on the OS not handing out the
/// same `pid` at the same nanosecond; that is a practical guarantee, not a
/// proof of global uniqueness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);
/// Process-local epoch pinned on the first allocation: this process's `pid` and
/// the nanoseconds at that first allocation. It uses the same `pid` plus
/// timestamp fact the client-shell boot id relies on to reject work from an
/// earlier server process.
static PROCESS_EPOCH: OnceLock<(u32, u128)> = OnceLock::new();

impl TerminalId {
    pub fn alloc() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let &(pid, first_alloc_nanos) =
            PROCESS_EPOCH.get_or_init(|| (std::process::id(), now.as_nanos()));
        let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        Self::from_parts(pid, first_alloc_nanos, now.as_micros(), counter)
    }

    fn from_parts(pid: u32, first_alloc_nanos: u128, micros: u128, counter: u64) -> Self {
        Self(format!(
            "term_{pid:x}_{first_alloc_nanos:x}_{micros:x}_{counter:x}"
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for TerminalId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn process_epoch_prefix() -> String {
        let (pid, first_alloc_nanos) = *PROCESS_EPOCH
            .get()
            .expect("allocating a terminal id pins the process epoch");
        format!("term_{pid:x}_{first_alloc_nanos:x}_")
    }

    #[test]
    fn terminal_ids_support_borrowed_lookup_without_changing_identity() {
        let id = TerminalId::alloc();
        let serialized = serde_json::to_string(&id).unwrap();
        let restored: TerminalId = serde_json::from_str(&serialized).unwrap();
        let mut terminals = HashMap::from([(id.clone(), 1)]);

        assert_eq!(terminals.get(restored.as_str()), Some(&1));
        assert_eq!(terminals.get("missing-terminal"), None);
        *terminals.get_mut(restored.as_str()).unwrap() = 2;
        assert_eq!(terminals.remove(id.as_str()), Some(2));
        assert!(terminals.is_empty());
    }

    #[test]
    fn terminal_ids_are_disjoint_across_boot_epochs() {
        // Same micros and counter: only the process epoch differs.
        assert_ne!(
            TerminalId::from_parts(100, 1000, 0x18f, 1),
            TerminalId::from_parts(101, 1000, 0x18f, 1)
        );
        assert_ne!(
            TerminalId::from_parts(100, 1000, 0x18f, 1),
            TerminalId::from_parts(100, 1001, 0x18f, 1)
        );
    }

    #[test]
    fn terminal_id_process_epoch_is_stable_and_present() {
        let first = TerminalId::alloc();
        let second = TerminalId::alloc();
        let prefix = process_epoch_prefix();

        assert!(first.as_str().starts_with(&prefix), "{first}");
        assert!(second.as_str().starts_with(&prefix), "{second}");
        assert!(first.as_str().starts_with("term_"));
        assert_ne!(first, second);
    }

    #[test]
    fn terminal_id_field_boundaries_are_unambiguous() {
        // The pre-epoch encoding concatenated its hex fields, so both of these
        // tuples rendered the same `term_18f11`. Separate fields cannot.
        assert_ne!(
            TerminalId::from_parts(7, 9, 0x18f, 0x11),
            TerminalId::from_parts(7, 9, 0x18f1, 0x1)
        );
        // The same hazard inside the process epoch itself.
        assert_ne!(
            TerminalId::from_parts(0x1, 0x2f, 5, 1),
            TerminalId::from_parts(0x12, 0xf, 5, 1)
        );
    }
}
