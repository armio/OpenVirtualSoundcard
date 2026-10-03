// XPC, dispatch and os_log helpers for the IPC between the OpenVirtualSoundcard daemon
// and its Core Audio driver.
//
// XPC and dispatch deliver events through blocks, which Rust cannot create
// without extra crates. This shim turns them into plain C callbacks. Every
// callback runs on one serial dispatch queue, "org.openvirtualsoundcard.ipc", so the
// Rust side never sees two of them at once. Everything else in XPC is
// ordinary C and is called from Rust directly.
//
// Copied from tools/macos-probe/shim/c (which stays as it is) and extended.

#ifndef OVSHIM_H
#define OVSHIM_H

#include <stdbool.h>
#include <stdint.h>
#include <xpc/xpc.h>

// What an XPC object is, as far as the Rust side cares.
enum {
    OVSHIM_DICTIONARY = 1,
    OVSHIM_INTERRUPTED = 2,     // the peer went away; the connection may come back
    OVSHIM_INVALID = 3,         // the connection can never work (no such service, cancelled)
    OVSHIM_TERMINATION_IMMINENT = 4,
    OVSHIM_OTHER = 5,
    OVSHIM_SHMEM = 6,
    OVSHIM_ARRAY = 7,
    OVSHIM_STRING = 8,
    // A new peer of a listener: passed as both `conn` and `event`, after the
    // peer has been resumed and before any of its messages.
    OVSHIM_CONNECTION = 9,
    OVSHIM_UINT64 = 10,
    OVSHIM_INT64 = 11,
    OVSHIM_BOOL = 12,
};

// Called on the shim's queue for every event on a connection.
typedef void (*ovshim_event_fn)(void *ctx, xpc_connection_t conn, xpc_object_t event);
// Called on the shim's queue with the reply to a message; `reply` may be an
// XPC error object.
typedef void (*ovshim_reply_fn)(void *ctx, xpc_object_t reply);
// Work submitted to the shim's queue.
typedef void (*ovshim_work_fn)(void *ctx);
// Called for each entry of a dictionary; returning false stops the walk.
typedef bool (*ovshim_apply_fn)(void *ctx, const char *key, xpc_object_t value);

int ovshim_kind(xpc_object_t object);

// Connects to a Mach service (flags as for
// xpc_connection_create_mach_service) and resumes the connection. Events go
// to `fn`.
xpc_connection_t ovshim_connect(const char *service, uint64_t flags, ovshim_event_fn fn, void *ctx);

// Registers a listener for a Mach service that launchd holds for this
// process. Every accepted peer is resumed, reported to `fn` with
// OVSHIM_CONNECTION, and its events go to `fn`. Events of the listener itself
// go to `fn` with the listener as `conn`.
xpc_connection_t ovshim_listen(const char *service, ovshim_event_fn fn, void *ctx);

// Like ovshim_listen, for an anonymous listener: `*out_endpoint` receives an
// endpoint (retained) that ovshim_connect_endpoint connects to.
xpc_connection_t ovshim_listen_anonymous(ovshim_event_fn fn, void *ctx, xpc_endpoint_t *out_endpoint);
xpc_connection_t ovshim_connect_endpoint(xpc_endpoint_t endpoint, ovshim_event_fn fn, void *ctx);

// Sends `msg` and delivers the reply (or an error) to `fn` exactly once.
void ovshim_send_with_reply(xpc_connection_t conn, xpc_object_t msg, ovshim_reply_fn fn, void *ctx);

// Runs `fn` on the shim's queue, now or after `delay_ns` (at most a year).
void ovshim_async(ovshim_work_fn fn, void *ctx);
void ovshim_after(uint64_t delay_ns, ovshim_work_fn fn, void *ctx);

// xpc_dictionary_apply with a function. Returns false if `fn` stopped it.
bool ovshim_dict_apply(xpc_object_t dict, ovshim_apply_fn fn, void *ctx);

// Logging through os_log. The first ovshim_log_init picks the subsystem and
// category; before it, messages go to the default log. `level` is an
// os_log_type_t. Messages are logged as %{public}s.
void ovshim_log_init(const char *subsystem, const char *category);
void ovshim_log(int level, const char *msg);

// Puts the calling thread in the time-constraint (real-time) class.
// Returns the kern_return_t.
int ovshim_make_realtime(uint32_t period_ns, uint32_t computation_ns, uint32_t constraint_ns);

#endif
