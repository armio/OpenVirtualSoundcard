// XPC, Mach and sandbox helpers for the macOS sandbox probe.
//
// XPC delivers events through blocks, which Rust cannot create without extra
// crates. This shim turns them into plain C callbacks. Everything else in XPC
// is ordinary C and is called from Rust directly.

#ifndef OVSHIM_H
#define OVSHIM_H

#include <stdint.h>
#include <sys/types.h>
#include <xpc/xpc.h>

// What an XPC object delivered to an event handler is.
enum {
    OVSHIM_DICTIONARY = 1,
    OVSHIM_INTERRUPTED = 2,     // the peer went away; the connection may come back
    OVSHIM_INVALID = 3,         // the connection can never work (no such service, cancelled)
    OVSHIM_TERMINATION_IMMINENT = 4,
    OVSHIM_OTHER = 5,
};

// Called on the shim's serial queue for every event on a connection.
typedef void (*ovshim_event_fn)(void *ctx, xpc_connection_t conn, xpc_object_t event);

int ovshim_kind(xpc_object_t object);

// Connects to a Mach service and resumes the connection. Events go to `fn`.
xpc_connection_t ovshim_connect(const char *service, ovshim_event_fn fn, void *ctx);

// Registers a listener for a Mach service that launchd holds for this
// process. Every accepted peer is resumed and its events go to `fn`.
xpc_connection_t ovshim_listen(const char *service, ovshim_event_fn fn, void *ctx);

// bootstrap_look_up() without XPC. Returns the kern_return_t.
int ovshim_bootstrap_look_up(const char *service);

// sandbox_check(getpid(), NULL, SANDBOX_FILTER_NONE): > 0 when sandboxed.
int ovshim_sandboxed(void);

// sandbox_check() for an operation filtered by a global name or a path,
// without logging a violation. 0 allowed, 1 denied, -1 unknown.
int ovshim_sandbox_allows_name(const char *operation, const char *name);
int ovshim_sandbox_allows_path(const char *operation, const char *path);

// Puts the calling thread in the time-constraint (real-time) class.
// Returns the kern_return_t.
int ovshim_make_realtime(uint32_t period_ns, uint32_t computation_ns, uint32_t constraint_ns);

const char *ovshim_progname(void);

#endif
