// Copyright 2025 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
use ipc_channel::ipc::IpcOneShotServer;
#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
use std::{env, process};

// These integration tests may be run on their own by issuing:
// cargo test --test '*'

/// Test spawing a process which then acts as a client to a
/// one-shot server in the parent process.
#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
#[test]
fn spawn_one_shot_server_client() {
    let (server, token) =
        IpcOneShotServer::<String>::new().expect("Failed to create IPC one-shot server.");

    let child = spawn_client_test_helper(token, "test message".to_string());

    let (_rx, msg) = server.accept().expect("accept failed");
    assert_eq!("test message", msg);

    await_child_process(child);
}
/// Test spawing two processes which then act as clients to a
/// server in the parent process.
#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
#[test]
fn spawn_multi_shot_server_clients() {
    let (server, token) =
        IpcOneShotServer::<String>::new().expect("Failed to create IPC one-shot server.");

    let child1 = spawn_client_test_helper(token.clone(), "test message 1".to_string());
    await_child_process(child1);

    let (rx, msg1) = server.accept().expect("accept failed");
    assert_eq!("test message 1", msg1);
    
    let child2 = spawn_client_test_helper(token, "test message 2".to_string());  
    await_child_process(child2);

    let msg2 = rx.recv().expect("failed to receive message 2");
    assert_eq!("test message 2", msg2);
}

#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
fn spawn_client_test_helper(server_name: String, message: String) -> process::Child {
    let executable_path: String = env!("CARGO_BIN_EXE_spawn_client_test_helper").to_string();
    process::Command::new(executable_path)
        .arg(server_name)
        .arg(message)
        .spawn()
        .expect("Failed to start child process")
}

#[cfg(not(any(feature = "force-inprocess", target_os = "android", target_os = "ios")))]
fn await_child_process(mut child: process::Child) {
    let result = child.wait().expect("wait for child process failed");
    assert!(
        result.success(),
        "child process failed with exit status code {}",
        result.code().expect("exit status code not available")
    );
}