#include "ovshim.h"

#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <os/log.h>
#include <stdatomic.h>

static dispatch_queue_t ovshim_queue(void) {
    static dispatch_once_t once;
    static dispatch_queue_t queue;
    dispatch_once(&once, ^{
        queue = dispatch_queue_create("org.openvirtualsoundcard.ipc", DISPATCH_QUEUE_SERIAL);
    });
    return queue;
}

int ovshim_kind(xpc_object_t object) {
    if (object == NULL) return OVSHIM_OTHER;
    xpc_type_t type = xpc_get_type(object);
    if (type == XPC_TYPE_DICTIONARY) return OVSHIM_DICTIONARY;
    if (object == XPC_ERROR_CONNECTION_INTERRUPTED) return OVSHIM_INTERRUPTED;
    if (object == XPC_ERROR_CONNECTION_INVALID) return OVSHIM_INVALID;
    if (object == XPC_ERROR_TERMINATION_IMMINENT) return OVSHIM_TERMINATION_IMMINENT;
    if (type == XPC_TYPE_SHMEM) return OVSHIM_SHMEM;
    if (type == XPC_TYPE_ARRAY) return OVSHIM_ARRAY;
    if (type == XPC_TYPE_STRING) return OVSHIM_STRING;
    if (type == XPC_TYPE_CONNECTION) return OVSHIM_CONNECTION;
    if (type == XPC_TYPE_UINT64) return OVSHIM_UINT64;
    if (type == XPC_TYPE_INT64) return OVSHIM_INT64;
    if (type == XPC_TYPE_BOOL) return OVSHIM_BOOL;
    return OVSHIM_OTHER;
}

static void ovshim_handle(xpc_connection_t conn, ovshim_event_fn fn, void *ctx) {
    xpc_connection_set_event_handler(conn, ^(xpc_object_t event) {
        fn(ctx, conn, event);
    });
    xpc_connection_resume(conn);
}

xpc_connection_t ovshim_connect(const char *service, uint64_t flags, ovshim_event_fn fn, void *ctx) {
    xpc_connection_t conn = xpc_connection_create_mach_service(service, ovshim_queue(), flags);
    if (conn == NULL) return NULL;
    ovshim_handle(conn, fn, ctx);
    return conn;
}

// Peers run on the shim's queue too, so none of a peer's events can be
// handled before the listener's handler has reported it.
static void ovshim_serve(xpc_connection_t listener, ovshim_event_fn fn, void *ctx) {
    xpc_connection_set_event_handler(listener, ^(xpc_object_t event) {
        if (xpc_get_type(event) == XPC_TYPE_CONNECTION) {
            xpc_connection_t peer = (xpc_connection_t)event;
            xpc_connection_set_target_queue(peer, ovshim_queue());
            ovshim_handle(peer, fn, ctx);
            fn(ctx, peer, peer);
        } else {
            fn(ctx, listener, event);
        }
    });
    xpc_connection_resume(listener);
}

xpc_connection_t ovshim_listen(const char *service, ovshim_event_fn fn, void *ctx) {
    xpc_connection_t listener = xpc_connection_create_mach_service(
        service, ovshim_queue(), XPC_CONNECTION_MACH_SERVICE_LISTENER);
    if (listener == NULL) return NULL;
    ovshim_serve(listener, fn, ctx);
    return listener;
}

xpc_connection_t ovshim_listen_anonymous(ovshim_event_fn fn, void *ctx, xpc_endpoint_t *out_endpoint) {
    xpc_connection_t listener = xpc_connection_create(NULL, ovshim_queue());
    if (listener == NULL) return NULL;
    ovshim_serve(listener, fn, ctx);
    *out_endpoint = xpc_endpoint_create(listener);
    return listener;
}

xpc_connection_t ovshim_connect_endpoint(xpc_endpoint_t endpoint, ovshim_event_fn fn, void *ctx) {
    xpc_connection_t conn = xpc_connection_create_from_endpoint(endpoint);
    if (conn == NULL) return NULL;
    xpc_connection_set_target_queue(conn, ovshim_queue());
    ovshim_handle(conn, fn, ctx);
    return conn;
}

void ovshim_send_with_reply(xpc_connection_t conn, xpc_object_t msg, ovshim_reply_fn fn, void *ctx) {
    xpc_connection_send_message_with_reply(conn, msg, ovshim_queue(), ^(xpc_object_t reply) {
        fn(ctx, reply);
    });
}

void ovshim_async(ovshim_work_fn fn, void *ctx) {
    dispatch_async_f(ovshim_queue(), ctx, fn);
}

void ovshim_after(uint64_t delay_ns, ovshim_work_fn fn, void *ctx) {
    // Far beyond any delay the IPC uses, and safe from overflow in
    // dispatch_time (DISPATCH_TIME_FOREVER would never fire).
    const uint64_t max_delay = 365ull * 24 * 3600 * NSEC_PER_SEC;
    if (delay_ns > max_delay) delay_ns = max_delay;
    dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, (int64_t)delay_ns), ovshim_queue(), ctx, fn);
}

bool ovshim_dict_apply(xpc_object_t dict, ovshim_apply_fn fn, void *ctx) {
    return xpc_dictionary_apply(dict, ^bool(const char *key, xpc_object_t value) {
        return fn(ctx, key, value);
    });
}

static _Atomic(os_log_t) ovshim_log_handle;

void ovshim_log_init(const char *subsystem, const char *category) {
    if (atomic_load(&ovshim_log_handle) != NULL) return;
    os_log_t log = os_log_create(subsystem, category);
    os_log_t none = NULL;
    // A concurrent first call wins; the loser's handle is kept alive (log
    // handles are never freed in practice).
    atomic_compare_exchange_strong(&ovshim_log_handle, &none, log);
}

void ovshim_log(int level, const char *msg) {
    os_log_t log = atomic_load(&ovshim_log_handle);
    if (log == NULL) log = OS_LOG_DEFAULT;
    os_log_with_type(log, (os_log_type_t)level, "%{public}s", msg);
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
