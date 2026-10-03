#include "ovshim.h"

#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <servers/bootstrap.h>
#include <stdlib.h>
#include <unistd.h>

// Private but stable libsystem_sandbox interface (used the same way by
// WebKit and Chromium).
enum sandbox_filter_type {
    SANDBOX_FILTER_NONE = 0,
    SANDBOX_FILTER_PATH = 1,
    SANDBOX_FILTER_GLOBAL_NAME = 2,
};
extern const enum sandbox_filter_type SANDBOX_CHECK_NO_REPORT;
extern int sandbox_check(pid_t pid, const char *operation, enum sandbox_filter_type type, ...);

static dispatch_queue_t ovshim_queue(void) {
    static dispatch_once_t once;
    static dispatch_queue_t queue;
    dispatch_once(&once, ^{
        queue = dispatch_queue_create("org.openvirtualsoundcard.probe.xpc", DISPATCH_QUEUE_SERIAL);
    });
    return queue;
}

int ovshim_kind(xpc_object_t object) {
    xpc_type_t type = xpc_get_type(object);
    if (type == XPC_TYPE_DICTIONARY) return OVSHIM_DICTIONARY;
    if (object == XPC_ERROR_CONNECTION_INTERRUPTED) return OVSHIM_INTERRUPTED;
    if (object == XPC_ERROR_CONNECTION_INVALID) return OVSHIM_INVALID;
    if (object == XPC_ERROR_TERMINATION_IMMINENT) return OVSHIM_TERMINATION_IMMINENT;
    return OVSHIM_OTHER;
}

xpc_connection_t ovshim_connect(const char *service, ovshim_event_fn fn, void *ctx) {
    xpc_connection_t conn = xpc_connection_create_mach_service(service, ovshim_queue(), 0);
    if (conn == NULL) return NULL;
    xpc_connection_set_event_handler(conn, ^(xpc_object_t event) {
        fn(ctx, conn, event);
    });
    xpc_connection_resume(conn);
    return conn;
}

xpc_connection_t ovshim_listen(const char *service, ovshim_event_fn fn, void *ctx) {
    xpc_connection_t listener = xpc_connection_create_mach_service(
        service, ovshim_queue(), XPC_CONNECTION_MACH_SERVICE_LISTENER);
    if (listener == NULL) return NULL;
    xpc_connection_set_event_handler(listener, ^(xpc_object_t event) {
        if (xpc_get_type(event) == XPC_TYPE_CONNECTION) {
            xpc_connection_t peer = (xpc_connection_t)event;
            xpc_connection_set_event_handler(peer, ^(xpc_object_t message) {
                fn(ctx, peer, message);
            });
            xpc_connection_resume(peer);
        } else {
            fn(ctx, listener, event);
        }
    });
    xpc_connection_resume(listener);
    return listener;
}

int ovshim_bootstrap_look_up(const char *service) {
    mach_port_t port = MACH_PORT_NULL;
    kern_return_t kr = bootstrap_look_up(bootstrap_port, service, &port);
    if (kr == KERN_SUCCESS && port != MACH_PORT_NULL) {
        mach_port_deallocate(mach_task_self(), port);
    }
    return kr;
}

int ovshim_sandboxed(void) {
    return sandbox_check(getpid(), NULL, SANDBOX_FILTER_NONE);
}

int ovshim_sandbox_allows_name(const char *operation, const char *name) {
    int r = sandbox_check(getpid(), operation, SANDBOX_FILTER_GLOBAL_NAME | SANDBOX_CHECK_NO_REPORT, name);
    return r == 0 ? 0 : (r > 0 ? 1 : -1);
}

int ovshim_sandbox_allows_path(const char *operation, const char *path) {
    int r = sandbox_check(getpid(), operation, SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT, path);
    return r == 0 ? 0 : (r > 0 ? 1 : -1);
}

int ovshim_make_realtime(uint32_t period_ns, uint32_t computation_ns, uint32_t constraint_ns) {
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    // Absolute-time units = ns * denom / numer.
    thread_time_constraint_policy_data_t policy = {
        .period = (uint32_t)((uint64_t)period_ns * tb.denom / tb.numer),
        .computation = (uint32_t)((uint64_t)computation_ns * tb.denom / tb.numer),
        .constraint = (uint32_t)((uint64_t)constraint_ns * tb.denom / tb.numer),
        .preemptible = 1,
    };
    mach_port_t thread = mach_thread_self();
    kern_return_t kr = thread_policy_set(thread, THREAD_TIME_CONSTRAINT_POLICY,
                                         (thread_policy_t)&policy,
                                         THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    mach_port_deallocate(mach_task_self(), thread);
    return kr;
}

const char *ovshim_progname(void) {
    return getprogname();
}
