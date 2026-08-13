//! Software this device manages on the server's behalf.
//!
//! One so far, and it is this client updating itself: the server names a version, a url and a hash,
//! and the device fetches, verifies and installs it. That lives in [`crate::update`] because it ends
//! by replacing the running process; what is here is the dispatch and the words used to describe a
//! component's state.
//!
//! There used to be a second one — `sonn-beoremote`, our build of Bang & Olufsen's patched BlueZ,
//! installed as its own GPLv2 daemon that took over the Bluetooth adapter — because a Beoremote One
//! on stock BlueZ is only a keyboard and cannot be given menus. That is no longer true: the client
//! serves the remote's GATT service itself and reads its keys from the kernel's input devices, so
//! there is nothing left to install and no vendor daemon in the path at all.

use crate::models::{ComponentStatus, DesiredComponent};

/// How a component's state is spelled where the server and the admin screen read it.
pub const STATE_INSTALLED: &str = "installed";
pub const STATE_FAILED: &str = "failed";

/// Bring the installed components in line with what the server asked for.
///
/// Returns the status of everything it was asked about, so the server can show what a device is
/// running. `busy` says whether this device is playing, which the client's own update waits for
/// rather than interrupting the music.
pub async fn reconcile(desired: &[DesiredComponent], busy: bool) -> Vec<ComponentStatus> {
    let mut reports = Vec::new();

    for component in desired {
        if component.name == crate::update::SONN_CLIENT {
            reports.push(crate::update::reconcile(component, busy).await);
            continue;
        }
        // Refused rather than guessed at: installing software on someone's device off the back of a
        // name this build does not recognise is not a thing to be relaxed about.
        reports.push(ComponentStatus {
            name: component.name.clone(),
            version: None,
            state: STATE_FAILED.to_string(),
            last_error: Some("unknown component".to_string()),
        });
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_component_this_build_does_not_know_is_refused() {
        let desired = vec![DesiredComponent {
            name: "something-else".to_string(),
            version: Some("1.0".to_string()),
            url: Some("https://example.invalid/thing.tar.gz".to_string()),
            sha256: None,
            enabled: None,
        }];
        let reports = reconcile(&desired, false).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, STATE_FAILED);
        assert_eq!(reports[0].last_error.as_deref(), Some("unknown component"));
    }

    #[tokio::test]
    async fn nothing_asked_for_is_nothing_reported() {
        assert!(reconcile(&[], false).await.is_empty());
    }
}
