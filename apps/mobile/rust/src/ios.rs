use crate::{EVENT_SENDER, NODE, RUNTIME, SUBSCRIPTIONS};
use futures::future::join_all;
use objc::{msg_send, runtime::Object, sel, sel_impl};
use objc_foundation::{INSString, NSString};
use objc_id::Id;
use rspc::internal::jsonrpc::{handle_json_rpc, Request, Sender, SubscriptionMap};
use sd_core::Node;
use serde_json::Value;
use std::{
	ffi::{CStr, CString},
	os::raw::{c_char, c_void},
	panic,
};
use tokio::sync::mpsc::unbounded_channel;

extern "C" {
	fn get_data_directory() -> *const c_char;
	fn call_resolve(resolve: *const c_void, result: *const c_char);
}

// This struct wraps the function pointer which represent a Javascript Promise. We wrap the
// function pointers in a struct so we can unsafely assert to Rust that they are `Send`.
// We know they are send as we have ensured Objective-C won't deallocate the function pointer
// until `call_resolve` is called.
struct RNPromise(*const c_void);

unsafe impl Send for RNPromise {}

impl RNPromise {
	// resolve the promise
	unsafe fn resolve(self, result: CString) {
		call_resolve(self.0, result.as_ptr());
	}
}

#[no_mangle]
pub unsafe extern "C" fn register_core_event_listener(id: *mut Object) {
	let result = panic::catch_unwind(|| {
		let id = Id::<Object>::from_ptr(id);

		let (tx, mut rx) = unbounded_channel();
		let _ = EVENT_SENDER.set(tx);

		RUNTIME.spawn(async move {
			while let Some(event) = rx.recv().await {
				let data = match serde_json::to_string(&event) {
					Ok(json) => json,
					Err(err) => {
						println!("Failed to serialize event: {}", err);
						continue;
					}
				};
				let data = NSString::from_str(&data);
				let _: () = msg_send![id, sendCoreEvent: data];
			}
		});
	});

	if let Err(err) = result {
		// TODO: Send rspc error or something here so we can show this in the UI.
		// TODO: Maybe reinitialise the core cause it could be in an invalid state?
		println!("Error in register_core_event_listener: {:?}", err);
	}
}

#[no_mangle]
pub unsafe extern "C" fn sd_core_msg(query: *const c_char, resolve: *const c_void) {
	let result = panic::catch_unwind(|| {
		// This string is cloned to the Rust heap. This is important as Objective-C may remove the query once this function completions but prior to the async block finishing.
		let query = CStr::from_ptr(query).to_str().unwrap().to_string();

		let resolve = RNPromise(resolve);
		RUNTIME.spawn(async move {
			let reqs =
				match serde_json::from_str::<Value>(&query).and_then(|v| match v.is_array() {
					true => serde_json::from_value::<Vec<Request>>(v),
					false => serde_json::from_value::<Request>(v).map(|v| vec![v]),
				}) {
					Ok(v) => v,
					Err(err) => {
						println!("failed to decode JSON-RPC request: {}", err); // Don't use tracing here because it's before the `Node` is initialised which sets that config!

						resolve.resolve(
							CString::new(serde_json::to_vec(&(vec![] as Vec<Request>)).unwrap())
								.unwrap(),
						); // TODO: Proper error handling
						return;
					}
				};

			let resps = join_all(reqs.into_iter().map(|request| async move {
				let node = &mut *NODE.lock().await;
				let (node, router) = match node {
					Some(node) => node.clone(),
					None => {
						let data_dir = CStr::from_ptr(get_data_directory())
							.to_str()
							.unwrap()
							.to_string();
						let new_node = Node::new(data_dir).await.unwrap();
						node.replace(new_node.clone());
						new_node
					}
				};

				let mut channel = EVENT_SENDER.get().unwrap().clone();
				let mut resp = Sender::ResponseAndChannel(None, &mut channel);
				handle_json_rpc(
					node.get_request_context(),
					request,
					&router,
					&mut resp,
					&mut SubscriptionMap::Mutex(&SUBSCRIPTIONS),
				)
				.await;

				match resp {
					Sender::ResponseAndChannel(resp, _) => resp,
					_ => unreachable!(),
				}
			}))
			.await;

			resolve.resolve(
				CString::new(
					serde_json::to_vec(&resps.into_iter().filter_map(|v| v).collect::<Vec<_>>())
						.unwrap(),
				)
				.unwrap(),
			);
		});
	});

	if let Err(err) = result {
		// TODO: Send rspc error or something here so we can show this in the UI.
		// TODO: Maybe reinitialise the core cause it could be in an invalid state?
		println!("Error in sd_core_msg: {:?}", err);
	}
}
