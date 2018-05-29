// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Ethcore client application.

#![warn(missing_docs)]
#![cfg_attr(feature = "cargo-clippy", deny(clippy, clippy_pedantic))]

extern crate ctrlc;
extern crate dir;
extern crate fdlimit;
#[macro_use]
extern crate log;
extern crate panic_hook;
extern crate parity;
extern crate parking_lot;

#[cfg(windows)] extern crate winapi;

use ctrlc::CtrlC;
use dir::default_hypervisor_path;
use fdlimit::raise_fd_limit;
use parity::{start, ExecutionAction};
use parking_lot::{Condvar, Mutex};
use std::fs::{remove_file, metadata, File, create_dir_all};
use std::io::{self as stdio, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{process, env, ffi::OsString};

const PLEASE_RESTART_EXIT_CODE: i32 = 69;

#[derive(Debug)]
enum Error {
	BinaryNotFound,
	StatusCode(i32),
	UnknownStatusCode,
}

fn update_path(name: &str) -> PathBuf {
	let mut dest = default_hypervisor_path();
	dest.push(name);
	dest
}

fn latest_exe_path() -> Result<PathBuf, Error> {
	File::open(update_path("latest")).and_then(|mut f| { 
			let mut exe = String::new(); 
			println!("latest_exe: {:?}", f); 
			f.read_to_string(&mut exe).map(|_| update_path(&exe))
	}).or(Err(Error::BinaryNotFound))

}

fn latest_binary_is_newer(current_binary: &Option<PathBuf>, latest_binary: &Option<PathBuf>) -> bool {
	match (
		current_binary
			.as_ref()
			.and_then(|p| metadata(p.as_path()).ok())
			.and_then(|m| m.modified().ok()),
		latest_binary
			.as_ref()
			.and_then(|p| metadata(p.as_path()).ok())
			.and_then(|m| m.modified().ok())
	) {
			(Some(latest_exe_time), Some(this_exe_time)) if latest_exe_time > this_exe_time => true,
			_ => false,
	}
}

fn set_spec_name_override(spec_name: & str) {
	if let Err(e) = create_dir_all(default_hypervisor_path())
		.and_then(|_| File::create(update_path("spec_name_override"))
		.and_then(|mut f| f.write_all(spec_name.as_bytes())))
	{
		warn!("Couldn't override chain spec: {} at {:?}", e, update_path("spec_name_override"));
	}
}

fn take_spec_name_override() -> Option<String> {
	let p = update_path("spec_name_override");
	let r = File::open(p.clone())
		.ok()
		.and_then(|mut f| { 
			let mut spec_name = String::new(); 
			f.read_to_string(&mut spec_name).ok().map(|_| spec_name) 
		});
	let _ = remove_file(p);
	r
}

#[cfg(windows)]
fn global_cleanup() {
	// We need to cleanup all sockets before spawning another Parity process. This makes sure everything is cleaned up.
	// The loop is required because of internal reference counter for winsock dll. We don't know how many crates we use do
	// initialize it. There's at least 2 now.
	for _ in 0.. 10 {
		unsafe { ::winapi::um::winsock2::WSACleanup(); }
	}
}

#[cfg(not(windows))]
fn global_init() {}

#[cfg(windows)]
fn global_init() {
	// When restarting in the same process this reinits windows sockets.
	unsafe {
		const WS_VERSION: u16 = 0x202;
		let mut wsdata: ::winapi::um::winsock2::WSADATA = ::std::mem::zeroed();
		::winapi::um::winsock2::WSAStartup(WS_VERSION, &mut wsdata);
	}
}

#[cfg(not(windows))]
fn global_cleanup() {}

// Starts ~/.parity-updates/parity and returns the code it exits with.
fn run_parity() -> Result<(), Error> {
	global_init();
	let prefix = vec![OsString::from("--can-restart"), OsString::from("--force-direct")];
	
	let res: Result<(), Error> = latest_exe_path()
		.and_then(|exe| process::Command::new(exe)
		.args(&(env::args_os().skip(1).chain(prefix.into_iter()).collect::<Vec<_>>()))
		.status()
		.ok()
		.map_or(Err(Error::UnknownStatusCode), |es| {
			match es.code() {

				// Process success
				Some(0) => Ok(()),

				// Process error code `c`
				Some(c) => Err(Error::StatusCode(c)),
				
				// Unknown error, couldn't determine error code
				_ => Err(Error::UnknownStatusCode),
			}
		})
	);	

	global_cleanup();

	res
}

// Run `locally installed version` of parity (i.e, not if any is installed via `parity-updater`)
// Returns the exit error code.
fn main_direct(force_can_restart: bool) -> i32 {
	global_init();

	let mut conf = {
		let args = std::env::args().collect::<Vec<_>>();
		parity::Configuration::parse_cli(&args).unwrap_or_else(|e| e.exit())
	};

	if let Some(spec_override) = take_spec_name_override() {
		conf.args.flag_testnet = false;
		conf.args.arg_chain = spec_override;
	}

	let can_restart = force_can_restart || conf.args.flag_can_restart;

	// increase max number of open files
	raise_fd_limit();

	let exit = Arc::new((Mutex::new((false, None)), Condvar::new()));

	let exec = if can_restart {
		let e1 = exit.clone();
		let e2 = exit.clone();
		start(conf,
			move |new_chain: String| { *e1.0.lock() = (true, Some(new_chain)); e1.1.notify_all(); },
			move || { *e2.0.lock() = (true, None); e2.1.notify_all(); })
	} else {
		trace!(target: "mode", "Not hypervised: not setting exit handlers.");
		start(conf, move |_| {}, move || {})
	};

	let res = match exec {
		Ok(result) => match result {
			ExecutionAction::Instant(Some(s)) => { println!("{}", s); 0 },
			ExecutionAction::Instant(None) => 0,
			ExecutionAction::Running(client) => {
				CtrlC::set_handler({
					let e = exit.clone();
					move || { e.1.notify_all(); }
				});

				// Wait for signal
				let mut lock = exit.0.lock();
				exit.1.wait(&mut lock);

				client.shutdown();

				match &*lock {
					(true, ref spec_name_override) => {
						if let Some(ref spec_name) = spec_name_override {
							set_spec_name_override(spec_name);
						}
						PLEASE_RESTART_EXIT_CODE
					},
					_ => 0,
				}
			},
		},
		Err(err) => {
			writeln!(&mut stdio::stderr(), "{}", err).expect("StdErr available; qed");
			1
		},
	};

	global_cleanup();
	res
}

fn println_trace_main(s: String) {
	if env::var("RUST_LOG").ok().and_then(|s| s.find("main=trace")).is_some() {
		println!("{}", s);
	}
}

#[macro_export]
macro_rules! trace_main {
	($arg:expr) => (println_trace_main($arg.into()));
	($($arg:tt)*) => (println_trace_main(format!("{}", format_args!($($arg)*))));
}

fn main() {
	panic_hook::set();

	// the user has specified to run its originally installed binary (not via `parity-updater`)
	let force_direct = std::env::args().any(|arg| arg == "--force-direct");
	
	// absolute path to the current `binary`
	let exe_path = std::env::current_exe().ok();
	
	// the binary is named `target/xx/yy`
	let development = exe_path
		.as_ref()
		.and_then(|p| {
			p.parent()
				.and_then(|p| p.parent())
				.and_then(|p| p.file_name())
				.map(|n| n == "target")
		})
		.unwrap_or(false);
	
	// the binary is named `parity`
	let same_name = exe_path
		.as_ref()
		.map_or(false, |p| { 
			p.file_stem().map_or(false, |n| n == "parity") && p.extension().map_or(false, |ext| ext == "exe")
		});

	trace_main!("Starting up {} (force-direct: {}, development: {}, same-name: {})", 
				std::env::current_exe().ok().map_or_else(|| "<unknown>".into(), |x| format!("{}", x.display())), 
				force_direct, 
				development, 
				same_name);

	trace_main!("Starting up {} (force-direct: {}, development: {}, same-name: {})", 
				std::env::current_exe().ok().map_or_else(|| "<unknown>".into(), |x| format!("{}", x.display())), 
				force_direct, 
				development, 
				same_name);

	if !force_direct && !development && same_name {
		// Try to run the latest installed version of `parity`, 
		// upon failure it fails fall back into the locally installed version of `parity`
		// Everything run inside a loop, so we'll be able to restart from the child into a new version seamlessly.
		loop {
			// `Path` to the latest downloaded binary
			let latest_exe = latest_exe_path().ok();
			
			// `Latest´ binary exist
			let have_update = latest_exe.as_ref().map_or(false, |p| p.exists());
			
			// Current binary is not same as the latest binary
			let current_binary_not_latest = exe_path
				.as_ref()
				.map_or(false, |exe| latest_exe.as_ref()
				.map_or(false, |lexe| exe.canonicalize().ok() != lexe.canonicalize().ok()));

			// Downloaded `binary` is newer
			let update_is_newer = latest_binary_is_newer(&latest_exe, &exe_path);
			trace_main!("Starting... (have-update: {}, non-updated-current: {}, update-is-newer: {})", have_update, current_binary_not_latest, update_is_newer);

			let exit_code = if have_update && current_binary_not_latest && update_is_newer {
				trace_main!("Attempting to run latest update ({})...", 
							latest_exe.as_ref().expect("guarded by have_update; latest_exe must exist for have_update; qed").display());
				match run_parity() {
					Ok(_) => 0,
					Err(e)=> {
						trace_main!("Updated binary could not be executed: {:?}\n Failing back to local version", e); 
						main_direct(true)
					}
				}
			} else {
				trace_main!("No latest update. Attempting to direct...");
				main_direct(true)
			};
			trace_main!("Latest exited with {}", exit_code);
			if exit_code != PLEASE_RESTART_EXIT_CODE {
				trace_main!("Quitting...");
				process::exit(exit_code);
			}
			trace_main!("Rerunning...");
		}
	} else {
		trace_main!("Running direct");
		// Otherwise, we're presumably running the version we want. Just run and fall-through.
		process::exit(main_direct(false));
	}
}
