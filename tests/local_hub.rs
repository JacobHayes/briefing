//! This machine's own hub: one owner per state dir, discovery that a leftover file cannot fool,
//! replacement of an older client-started hub through its control endpoint, and settings that
//! carry over to the next hub.

use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use briefing::local_hub::{self, HubFile};

mod common;

use common::Machine;

/// The hub that owns `machine`'s state dir, once one answers.
async fn wait_running(machine: &Machine) -> HubFile {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(file) = local_hub::running(machine.state.path()).await {
            return file;
        }
        assert!(Instant::now() < deadline, "no hub came up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A client command on `machine`; it starts the machine's hub if none is running.
fn status(machine: &Machine) -> Output {
    let output = machine.command().args(["status", "--json"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    output
}

/// Whether process `pid` is gone, waiting a little for it to exit.
fn exits(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let probe = std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status();
        let alive = probe.unwrap().success();
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[tokio::test]
async fn a_second_hub_cannot_own_the_same_state_dir() {
    let machine = Machine::new();
    let mut first = machine.command().args(["serve", "--port", "0"]).stderr(Stdio::null()).spawn().unwrap();
    let owner = wait_running(&machine).await;
    // On another free port it could listen fine; the state dir is what it cannot have.
    let second = machine.command().args(["serve", "--port", "0"]).output().unwrap();
    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&second.stderr);
    // It names the hub that does.
    assert!(stderr.contains(&owner.origin), "{stderr}");
    // The control request shuts the owner down gracefully.
    machine.stop_hub();
    assert!(first.wait().unwrap().success());
}

#[tokio::test]
async fn a_leftover_hub_file_is_not_trusted() {
    // Another machine's live hub stands in for whatever now answers at a stale file's origin.
    let other = Machine::new();
    status(&other);
    let elsewhere = wait_running(&other).await;

    let machine = Machine::new();
    HubFile { instance: "stale".into(), ..elsewhere.clone() }.write(machine.state.path()).unwrap();
    assert!(local_hub::running(machine.state.path()).await.is_none(), "wrong instance at that origin");
    status(&machine);
    let own = wait_running(&machine).await;
    assert_ne!(own.origin, elsewhere.origin, "started its own hub rather than using the other one");
    assert_ne!(own.instance, "stale");
}

#[tokio::test]
async fn an_older_client_started_hub_is_replaced_through_its_control_endpoint() {
    let machine = Machine::new();
    status(&machine);
    let old = wait_running(&machine).await;
    assert!(old.on_demand, "a hub a client starts is marked as such");
    // Pretend it is an older release; the file is what a client compares against.
    HubFile { version: "0.0.1".into(), ..old.clone() }.write(machine.state.path()).unwrap();

    status(&machine);
    let new = wait_running(&machine).await;
    assert_ne!(new.instance, old.instance);
    assert_ne!(new.pid, old.pid);
    assert!(exits(old.pid), "the old hub exited after its control request");
}

#[tokio::test]
async fn a_hub_run_by_hand_is_never_replaced() {
    let machine = Machine::new();
    let mut hub = machine.command().args(["serve", "--port", "0"]).stderr(Stdio::null()).spawn().unwrap();
    let manual = wait_running(&machine).await;
    assert!(!manual.on_demand);
    HubFile { version: "0.0.1".into(), ..manual.clone() }.write(machine.state.path()).unwrap();
    status(&machine);
    assert_eq!(wait_running(&machine).await.instance, manual.instance, "the same hub still serves");
    machine.stop_hub();
    assert!(hub.wait().unwrap().success());
}

#[tokio::test]
async fn a_restarted_hub_comes_back_on_the_last_port() {
    let machine = Machine::with_fixed_port();
    status(&machine);
    let first = wait_running(&machine).await;
    assert_eq!(first.port, machine.port);

    // A running hub wins over a client's other port, and the client says so.
    let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let other = machine.command().env("BRIEFING_PORT", busy.to_string()).args(["status", "--json"]).output().unwrap();
    assert!(other.status.success());
    assert!(String::from_utf8_lossy(&other.stderr).contains(&first.port.to_string()), "warned about the port in use");
    assert_eq!(wait_running(&machine).await.instance, first.instance);

    // A client given no port brings the next hub back where the last one was, so its links work.
    machine.stop_hub();
    let output = machine.command().env_remove("BRIEFING_PORT").args(["status", "--json"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let second = wait_running(&machine).await;
    assert_ne!(second.instance, first.instance);
    assert_eq!(second.port, machine.port);
    assert_eq!(second.origin, first.origin);
}
