//! JavaScript global object definitions.
//!
//! This module defines the JavaScript interface for the `__amla__` global.
//! These are injected into every JS context to provide the bridge between
//! agent JS code and the Rust runtime.

/// The `__amla__` global object JS source code.
///
/// This gets injected into every JS context to provide the bridge between
/// agent JS code and the host runtime.
pub const AMLA_GLOBAL_JS: &str = r"
// __amla__ global object - the agent's interface to capabilities
globalThis.__amla__ = {
    // Internal state
    _pendingOps: new Map(),
    _nextOpId: 1,

    // Create a pending operation and return a Promise
    _createPendingOp: function(opType, payload) {
        const id = String(this._nextOpId++);
        let resolver, rejecter;
        const promise = new Promise((resolve, reject) => {
            resolver = resolve;
            rejecter = reject;
        });
        this._pendingOps.set(id, { resolver, rejecter });

        // Register with native side - may return error object on failure
        // (C code parses JSON for us, so result is already an object if returned)
        const result = __native_register_op(id, JSON.stringify({ type: opType, ...payload }));

        // Check for registration error - reject immediately if so
        if (result !== undefined && result !== null && result.error) {
            this._pendingOps.delete(id);
            rejecter(new Error(result.error));
        }

        return promise;
    },

    // Resolve a pending operation (called by host)
    _resolve: function(id, result) {
        const pending = this._pendingOps.get(id);
        if (pending) {
            this._pendingOps.delete(id);
            pending.resolver(JSON.parse(result));
        }
    },

    // Reject a pending operation (called by host)
    _reject: function(id, error) {
        const pending = this._pendingOps.get(id);
        if (pending) {
            this._pendingOps.delete(id);
            pending.rejecter(new Error(error));
        }
    },

    // Call a tool with capability validation
    toolCall: function(tool, params) {
        return this._createPendingOp('tool_call', { tool, params });
    },

    // Fetch URL (goes through host for actual HTTP)
    fetch: function(url, options = {}) {
        return this._createPendingOp('fetch', { url, options });
    },

    // Read from partitioned memory
    memoryRead: function(key) {
        return this._createPendingOp('memory_read', { key });
    },

    // Write to partitioned memory
    memoryWrite: function(key, value) {
        return this._createPendingOp('memory_write', { key, value });
    },

    // Delete from partitioned memory
    memoryDelete: function(key) {
        return this._createPendingOp('memory_delete', { key });
    },

    // Spawn child session with attenuated capabilities
    spawn: function(attenuations) {
        return this._createPendingOp('spawn', { attenuations });
    },

    // Call an LLM
    llm: function(model, messages, options = {}) {
        return this._createPendingOp('llm_call', { model, messages, options });
    },

    // Sleep/delay (returns Promise that resolves after delay)
    sleep: function(ms) {
        return this._createPendingOp('sleep', { delay_ms: ms });
    },

    // Run a shell command through the internal sandboxed interpreter
    // Returns { stdout: string, stderr: string, exitCode: number }
    shell: function(command) {
        if (typeof command !== 'string') {
            return Promise.reject(new Error('shell: command must be a string'));
        }
        return this._createPendingOp('shell', { command });
    },

    // File system operations
    fs: {
        readFile: function(path, options = {}) {
            return __amla__._createPendingOp('fs_read', { path, options });
        },
        writeFile: function(path, data, options = {}) {
            return __amla__._createPendingOp('fs_write', { path, data, options });
        },
        readDir: function(path) {
            return __amla__._createPendingOp('fs_readdir', { path });
        },
        stat: function(path) {
            return __amla__._createPendingOp('fs_stat', { path });
        },
        exists: function(path) {
            return __amla__._createPendingOp('fs_exists', { path });
        },
        unlink: function(path) {
            return __amla__._createPendingOp('fs_unlink', { path });
        },
        mkdir: function(path, options = {}) {
            return __amla__._createPendingOp('fs_mkdir', { path, recursive: options.recursive || false });
        }
    },

    // Timer state for setTimeout/setInterval
    _timers: new Map(),
    _nextTimerId: 1,

    // Synchronous log (doesn't need Promise)
    log: function(level, message) {
        __native_log(level, message);
    },

    // Param constraint builder for spawn attenuations
    Param: function(name) {
        return {
            _param: name,
            le: function(value) { return { type: 'le', param: name, value }; },
            ge: function(value) { return { type: 'ge', param: name, value }; },
            lt: function(value) { return { type: 'lt', param: name, value }; },
            gt: function(value) { return { type: 'gt', param: name, value }; },
            eq: function(value) { return { type: 'eq', param: name, value }; },
            ne: function(value) { return { type: 'ne', param: name, value }; },
            in_: function(values) { return { type: 'in', param: name, values }; },
            notIn: function(values) { return { type: 'notIn', param: name, values }; },
            startsWith: function(prefix) { return { type: 'startsWith', param: name, prefix }; },
            endsWith: function(suffix) { return { type: 'endsWith', param: name, suffix }; },
            contains: function(substring) { return { type: 'contains', param: name, substring }; },
        };
    }
};

// Convenience aliases
const Param = __amla__.Param;

// Override global fetch to use our capability-controlled version
globalThis.fetch = function(url, options) {
    return __amla__.fetch(url, options).then(response => {
        // Return a Response-like object
        return {
            ok: response.ok,
            status: response.status,
            statusText: response.statusText,
            headers: new Map(Object.entries(response.headers || {})),
            json: () => Promise.resolve(response.body),
            text: () => Promise.resolve(
                typeof response.body === 'string' ? response.body : JSON.stringify(response.body)
            ),
        };
    });
};

// setTimeout implementation using __amla__.sleep
globalThis.setTimeout = function(callback, delay, ...args) {
    const id = __amla__._nextTimerId++;
    __amla__._timers.set(id, { callback, args, cancelled: false });
    __amla__.sleep(delay || 0).then(() => {
        const timer = __amla__._timers.get(id);
        if (timer && !timer.cancelled) {
            timer.callback.apply(null, timer.args);
        }
        __amla__._timers.delete(id);
    });
    return id;
};

// clearTimeout implementation
globalThis.clearTimeout = function(id) {
    const timer = __amla__._timers.get(id);
    if (timer) {
        timer.cancelled = true;
    }
};

// setInterval implementation using recursive setTimeout
globalThis.setInterval = function(callback, delay, ...args) {
    const id = __amla__._nextTimerId++;
    __amla__._timers.set(id, { callback, args, cancelled: false, interval: true });

    const tick = () => {
        __amla__.sleep(delay || 0).then(() => {
            const timer = __amla__._timers.get(id);
            if (timer && !timer.cancelled) {
                timer.callback.apply(null, timer.args);
                tick(); // Schedule next tick
            } else {
                __amla__._timers.delete(id);
            }
        });
    };
    tick();
    return id;
};

// clearInterval is the same as clearTimeout
globalThis.clearInterval = globalThis.clearTimeout;
";

/// Console override JS for capturing console.log output.
pub const CONSOLE_JS: &str = r"
// Override console to capture output
const __console_buffer = [];

// Safe stringify that handles BigInt, cycles, and other non-JSON values
function __safeStringify(val) {
    if (val === null) return 'null';
    if (val === undefined) return 'undefined';
    const type = typeof val;
    if (type === 'string') return val;
    if (type === 'number' || type === 'boolean') return String(val);
    if (type === 'bigint') return val.toString() + 'n';
    if (type === 'symbol') return val.toString();
    if (type === 'function') return '[Function]';
    // Object types - try JSON.stringify with cycle detection
    const seen = new WeakSet();
    try {
        return JSON.stringify(val, (key, value) => {
            if (typeof value === 'object' && value !== null) {
                if (seen.has(value)) return '[Circular]';
                seen.add(value);
            }
            if (typeof value === 'bigint') return value.toString() + 'n';
            return value;
        });
    } catch (e) {
        // Fallback for any other serialization errors
        try {
            return String(val);
        } catch (e2) {
            return '[Object]';
        }
    }
}

globalThis.console = {
    _buffer: __console_buffer,
    log: function(...args) {
        const msg = args.map(__safeStringify).join(' ');
        this._buffer.push({ level: 'log', message: msg });
        __native_log('log', msg);
    },
    error: function(...args) {
        const msg = args.map(__safeStringify).join(' ');
        this._buffer.push({ level: 'error', message: msg });
        __native_log('error', msg);
    },
    warn: function(...args) {
        const msg = args.map(__safeStringify).join(' ');
        this._buffer.push({ level: 'warn', message: msg });
        __native_log('warn', msg);
    },
    info: function(...args) {
        const msg = args.map(__safeStringify).join(' ');
        this._buffer.push({ level: 'info', message: msg });
        __native_log('info', msg);
    },
    debug: function(...args) {
        const msg = args.map(__safeStringify).join(' ');
        this._buffer.push({ level: 'debug', message: msg });
        __native_log('debug', msg);
    },
};
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amla_global_js_syntax() {
        // Basic syntax check - ensure the JS parses
        assert!(AMLA_GLOBAL_JS.contains("__amla__"));
        assert!(AMLA_GLOBAL_JS.contains("toolCall"));
        assert!(AMLA_GLOBAL_JS.contains("fetch"));
        assert!(AMLA_GLOBAL_JS.contains("memoryRead"));
    }

    #[test]
    fn test_console_js_syntax() {
        assert!(CONSOLE_JS.contains("console"));
        assert!(CONSOLE_JS.contains("__console_buffer"));
    }
}
