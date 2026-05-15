/*
 * wrapper.c - Simplified FFI wrapper for QuickJS
 *
 * Provides a C API surface suitable for Rust FFI bindings.
 * Works identically for native and WASM (wasm32-wasip1) builds.
 *
 * Design principles:
 * - All value exchange via JSON strings (simple, portable)
 * - Opaque handles for runtime/context
 * - Callbacks use function pointers with user data
 * - Memory managed by caller (qjs_free_string for results)
 * - NO GLOBAL STATE: All state is per-runtime or per-context
 *   (stored via QuickJS opaque pointers for thread safety)
 */

#include "wrapper.h"
#include "quickjs.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Internal: Convert JSValue to JSON string. Returns NULL on error. */
static char *js_value_to_json(JSContext *ctx, JSValue val) {
  JSValue json;
  const char *str;
  char *result;
  size_t len;

  /* Use JSON.stringify for objects/arrays, direct conversion for primitives */
  if (JS_IsUndefined(val)) {
    return strdup("null"); /* JSON has no undefined */
  }
  if (JS_IsNull(val)) {
    return strdup("null");
  }
  if (JS_IsBool(val)) {
    return strdup(JS_ToBool(ctx, val) ? "true" : "false");
  }
  if (JS_VALUE_GET_TAG(val) == JS_TAG_INT) {
    int32_t i;
    JS_ToInt32(ctx, &i, val);
    char buf[32];
    snprintf(buf, sizeof(buf), "%d", i);
    return strdup(buf);
  }
  if (JS_TAG_IS_FLOAT64(JS_VALUE_GET_TAG(val))) {
    double d;
    JS_ToFloat64(ctx, &d, val);
    char buf[64];
    /* Handle special float values that aren't valid JSON */
    if (d != d) { /* NaN */
      return strdup("null");
    } else if (d == (1.0 / 0.0) || d == (-1.0 / 0.0)) { /* Infinity */
      return strdup("null");
    } else {
      /* Use %.17g for full double precision (17 significant digits) */
      snprintf(buf, sizeof(buf), "%.17g", d);
      return strdup(buf);
    }
  }
  if (JS_IsString(val)) {
    str = JS_ToCStringLen(ctx, &len, val);
    if (!str)
      return NULL;
    /* Need to JSON-escape the string.
     * Worst case: every char is a control byte -> \uXXXX (6 chars each)
     * Plus 2 for quotes and 1 for null terminator */
    result = (char *)malloc(len * 6 + 3);
    if (!result) {
      JS_FreeCString(ctx, str);
      return NULL;
    }
    char *p = result;
    *p++ = '"';
    for (size_t i = 0; i < len; i++) {
      char c = str[i];
      switch (c) {
      case '"':
        *p++ = '\\';
        *p++ = '"';
        break;
      case '\\':
        *p++ = '\\';
        *p++ = '\\';
        break;
      case '\n':
        *p++ = '\\';
        *p++ = 'n';
        break;
      case '\r':
        *p++ = '\\';
        *p++ = 'r';
        break;
      case '\t':
        *p++ = '\\';
        *p++ = 't';
        break;
      default:
        if ((unsigned char)c < 32) {
          p += sprintf(p, "\\u%04x", (unsigned char)c);
        } else {
          *p++ = c;
        }
      }
    }
    *p++ = '"';
    *p = '\0';
    JS_FreeCString(ctx, str);
    return result;
  }

  /* Handle BigInt specially - convert to string representation */
  if (JS_IsBigInt(ctx, val)) {
    str = JS_ToCString(ctx, val);
    if (!str)
      return strdup("null");
    /* Return as JSON string with the BigInt's decimal representation */
    size_t slen = strlen(str);
    result = (char *)malloc(slen + 3); /* quotes + null */
    if (!result) {
      JS_FreeCString(ctx, str);
      return strdup("null");
    }
    result[0] = '"';
    memcpy(result + 1, str, slen);
    result[slen + 1] = '"';
    result[slen + 2] = '\0';
    JS_FreeCString(ctx, str);
    return result;
  }

  /* Handle Symbol - convert to description string */
  if (JS_IsSymbol(val)) {
    JSValue desc = JS_GetPropertyStr(ctx, val, "description");
    if (JS_IsString(desc)) {
      str = JS_ToCString(ctx, desc);
      JS_FreeValue(ctx, desc);
      if (str) {
        size_t slen = strlen(str);
        result =
            (char *)malloc(slen + 12); /* "Symbol(" + ")" + quotes + null */
        if (result) {
          snprintf(result, slen + 12, "\"Symbol(%s)\"", str);
          JS_FreeCString(ctx, str);
          return result;
        }
        JS_FreeCString(ctx, str);
      }
    } else {
      JS_FreeValue(ctx, desc);
    }
    return strdup("\"Symbol()\"");
  }

  /* For objects/arrays, use JSON.stringify */
  /* Note: JSON.stringify returns undefined for functions, symbols, etc. */
  json = JS_JSONStringify(ctx, val, JS_UNDEFINED, JS_UNDEFINED);
  if (JS_IsException(json)) {
    /* Clear the exception and return a fallback */
    JSValue exc = JS_GetException(ctx);
    JS_FreeValue(ctx, exc);
    /* Try to get a string representation instead */
    JSValue str_val = JS_ToString(ctx, val);
    if (!JS_IsException(str_val)) {
      str = JS_ToCString(ctx, str_val);
      JS_FreeValue(ctx, str_val);
      if (str) {
        /* Return as JSON string */
        size_t slen = strlen(str);
        result = (char *)malloc(slen * 6 + 3);
        if (result) {
          char *p = result;
          *p++ = '"';
          for (size_t i = 0; i < slen; i++) {
            char c = str[i];
            switch (c) {
            case '"':
              *p++ = '\\';
              *p++ = '"';
              break;
            case '\\':
              *p++ = '\\';
              *p++ = '\\';
              break;
            case '\n':
              *p++ = '\\';
              *p++ = 'n';
              break;
            case '\r':
              *p++ = '\\';
              *p++ = 'r';
              break;
            case '\t':
              *p++ = '\\';
              *p++ = 't';
              break;
            default:
              if ((unsigned char)c < 32) {
                p += sprintf(p, "\\u%04x", (unsigned char)c);
              } else {
                *p++ = c;
              }
            }
          }
          *p++ = '"';
          *p = '\0';
          JS_FreeCString(ctx, str);
          return result;
        }
        JS_FreeCString(ctx, str);
      }
    } else {
      JS_FreeValue(ctx, JS_GetException(ctx));
    }
    return strdup("\"[Object]\"");
  }
  /* JSON.stringify returns undefined for non-serializable values like functions
   */
  if (JS_IsUndefined(json)) {
    return strdup("null");
  }
  str = JS_ToCString(ctx, json);
  JS_FreeValue(ctx, json);
  if (!str)
    return NULL;
  result = strdup(str);
  JS_FreeCString(ctx, str);
  return result;
}

/* Internal: Parse JSON string to JSValue */
static JSValue json_to_js_value(JSContext *ctx, const char *json) {
  if (!json || !*json) {
    return JS_UNDEFINED;
  }
  return JS_ParseJSON(ctx, json, strlen(json), "<json>");
}

/* Internal: Escape a string for JSON embedding (writes to buffer, returns bytes
 * written) */
static size_t json_escape_string(char *dest, size_t dest_size,
                                 const char *src) {
  if (!src)
    return 0;
  size_t written = 0;
  for (size_t i = 0; src[i] && written + 6 < dest_size; i++) {
    char c = src[i];
    switch (c) {
    case '"':
      dest[written++] = '\\';
      dest[written++] = '"';
      break;
    case '\\':
      dest[written++] = '\\';
      dest[written++] = '\\';
      break;
    case '\n':
      dest[written++] = '\\';
      dest[written++] = 'n';
      break;
    case '\r':
      dest[written++] = '\\';
      dest[written++] = 'r';
      break;
    case '\t':
      dest[written++] = '\\';
      dest[written++] = 't';
      break;
    default:
      if ((unsigned char)c < 32) {
        written += sprintf(dest + written, "\\u%04x", (unsigned char)c);
      } else {
        dest[written++] = c;
      }
    }
  }
  dest[written] = '\0';
  return written;
}

/* Internal: Get exception info as JSON */
static char *get_exception_json(JSContext *ctx) {
  JSValue exc = JS_GetException(ctx);
  if (JS_IsNull(exc) || JS_IsUndefined(exc)) {
    return NULL;
  }

  const char *msg = NULL;
  const char *stack = NULL;
  char *result = NULL;

  /* Get message */
  JSValue msg_val = JS_GetPropertyStr(ctx, exc, "message");
  if (JS_IsString(msg_val)) {
    msg = JS_ToCString(ctx, msg_val);
  }
  JS_FreeValue(ctx, msg_val);

  /* Get stack */
  JSValue stack_val = JS_GetPropertyStr(ctx, exc, "stack");
  if (JS_IsString(stack_val)) {
    stack = JS_ToCString(ctx, stack_val);
  }
  JS_FreeValue(ctx, stack_val);

  /* If no message, stringify the exception itself */
  if (!msg) {
    JSValue str_val = JS_ToString(ctx, exc);
    if (JS_IsString(str_val)) {
      msg = JS_ToCString(ctx, str_val);
    }
    JS_FreeValue(ctx, str_val);
  }

  /* Build JSON result with proper escaping */
  size_t msg_len = msg ? strlen(msg) : 0;
  size_t stack_len = stack ? strlen(stack) : 0;
  /* Worst case: each char becomes \uXXXX (6 chars) */
  size_t buf_size = msg_len * 6 + stack_len * 6 + 64;
  result = (char *)malloc(buf_size);
  if (result) {
    char *escaped_msg = (char *)malloc(msg_len * 6 + 1);
    char *escaped_stack = stack ? (char *)malloc(stack_len * 6 + 1) : NULL;

    if (escaped_msg) {
      json_escape_string(escaped_msg, msg_len * 6 + 1,
                         msg ? msg : "Unknown error");
    }
    if (escaped_stack && stack) {
      json_escape_string(escaped_stack, stack_len * 6 + 1, stack);
    }

    if (escaped_stack) {
      snprintf(result, buf_size, "{\"message\":\"%s\",\"stack\":\"%s\"}",
               escaped_msg ? escaped_msg : "Unknown error",
               escaped_stack ? escaped_stack : "");
    } else {
      snprintf(result, buf_size, "{\"message\":\"%s\"}",
               escaped_msg ? escaped_msg : "Unknown error");
    }

    free(escaped_msg);
    free(escaped_stack);
  }

  if (msg)
    JS_FreeCString(ctx, msg);
  if (stack)
    JS_FreeCString(ctx, stack);
  JS_FreeValue(ctx, exc);

  return result;
}

/* ========== Per-Runtime State ========== */

/*
 * Runtime state stored via JS_SetRuntimeOpaque.
 * This replaces the old global interrupt_states[] array.
 * Each runtime owns its state - no sharing, no hash collisions.
 */
typedef struct {
  uint64_t instruction_limit;
  uint64_t instruction_count;
  int interrupted;
} RuntimeState;

/* Get runtime state (always succeeds for valid runtime) */
static RuntimeState *get_runtime_state(JSRuntime *rt) {
  return (RuntimeState *)JS_GetRuntimeOpaque(rt);
}

/* Interrupt handler - receives state via runtime opaque pointer */
static int interrupt_handler(JSRuntime *rt, void *opaque) {
  (void)opaque; /* We get state from runtime opaque instead */
  RuntimeState *state = get_runtime_state(rt);
  if (!state)
    return 0;

  /* Check for manual interrupt */
  if (state->interrupted) {
    return 1; /* Interrupt */
  }

  /* Check instruction limit */
  if (state->instruction_limit > 0) {
    state->instruction_count++;
    if (state->instruction_count >= state->instruction_limit) {
      return 1; /* Interrupt */
    }
  }

  return 0; /* Continue */
}

/* ========== Per-Context State ========== */

/*
 * Context state stored via JS_SetContextOpaque.
 * This replaces the old global callback_registry[] and next_id.
 * Each context owns its callbacks and promise counter.
 */
#define MAX_CALLBACKS_PER_CONTEXT 64

typedef struct {
  /* Callback storage - per context, not global */
  QJS_CCallback callbacks[MAX_CALLBACKS_PER_CONTEXT];
  void *callback_data[MAX_CALLBACKS_PER_CONTEXT];
  int callback_count;

  /* Promise ID counter - per context */
  uint64_t next_promise_id;

  /* User's opaque data (forwarded via qjs_set/get_context_opaque) */
  void *user_opaque;
} ContextState;

/* Get context state (always succeeds for valid context) */
static ContextState *get_context_state(JSContext *ctx) {
  return (ContextState *)JS_GetContextOpaque(ctx);
}

/* Allocate a callback slot in this context's registry */
static int allocate_callback_slot(JSContext *ctx, QJS_CCallback callback,
                                  void *user_data) {
  ContextState *state = get_context_state(ctx);
  if (!state || state->callback_count >= MAX_CALLBACKS_PER_CONTEXT) {
    return -1;
  }

  int slot = state->callback_count++;
  state->callbacks[slot] = callback;
  state->callback_data[slot] = user_data;
  return slot;
}

/* Internal: C function trampoline for registered callbacks.
 * func_data[0] contains the callback slot ID as an integer.
 * Looks up callback in context's own state - no global access.
 */
static JSValue js_callback_trampoline(JSContext *ctx, JSValueConst this_val,
                                      int argc, JSValueConst *argv, int magic,
                                      JSValueConst *func_data) {
  (void)this_val;
  (void)magic;

  /* Get callback slot ID from func_data */
  int32_t slot_id;
  if (JS_ToInt32(ctx, &slot_id, func_data[0]) < 0) {
    return JS_ThrowInternalError(ctx, "Invalid callback slot");
  }

  ContextState *state = get_context_state(ctx);
  if (!state || slot_id < 0 || slot_id >= state->callback_count) {
    return JS_ThrowInternalError(ctx, "Invalid callback slot");
  }

  QJS_CCallback callback = state->callbacks[slot_id];
  void *user_data = state->callback_data[slot_id];

  /* Build JSON array of arguments */
  JSValue args_array = JS_NewArray(ctx);
  for (int i = 0; i < argc; i++) {
    JS_SetPropertyUint32(ctx, args_array, i, JS_DupValue(ctx, argv[i]));
  }
  char *args_json = js_value_to_json(ctx, args_array);
  JS_FreeValue(ctx, args_array);

  if (!args_json) {
    return JS_ThrowInternalError(ctx, "Failed to serialize arguments");
  }

  /* Call the callback */
  char *result_json = callback(args_json, user_data);
  qjs_free_string(args_json);

  if (!result_json) {
    return JS_UNDEFINED;
  }

  /* Parse result JSON */
  JSValue result = json_to_js_value(ctx, result_json);
  qjs_free_string(result_json);
  return result;
}

/* ========== Public API: Runtime ========== */

QJS_Runtime *qjs_new_runtime(void) {
  JSRuntime *rt = JS_NewRuntime();
  if (!rt)
    return NULL;

  /* Allocate per-runtime state */
  RuntimeState *state = (RuntimeState *)calloc(1, sizeof(RuntimeState));
  if (!state) {
    JS_FreeRuntime(rt);
    return NULL;
  }

  JS_SetRuntimeOpaque(rt, state);
  return (QJS_Runtime *)rt;
}

void qjs_free_runtime(QJS_Runtime *rt) {
  if (!rt)
    return;

  RuntimeState *state = get_runtime_state((JSRuntime *)rt);
  free(state); /* Free per-runtime state */

  JS_FreeRuntime((JSRuntime *)rt);
}

void qjs_set_memory_limit(QJS_Runtime *rt, size_t limit) {
  if (rt) {
    JS_SetMemoryLimit((JSRuntime *)rt, limit);
  }
}

void qjs_set_max_stack_size(QJS_Runtime *rt, size_t size) {
  if (rt) {
    JS_SetMaxStackSize((JSRuntime *)rt, size);
  }
}

void qjs_set_instruction_limit(QJS_Runtime *rt, uint64_t instruction_limit) {
  if (!rt)
    return;

  RuntimeState *state = get_runtime_state((JSRuntime *)rt);
  if (!state)
    return;

  state->instruction_limit = instruction_limit;
  state->instruction_count = 0;
  state->interrupted = 0;

  if (instruction_limit > 0) {
    JS_SetInterruptHandler((JSRuntime *)rt, interrupt_handler, NULL);
  } else {
    JS_SetInterruptHandler((JSRuntime *)rt, NULL, NULL);
  }
}

void qjs_interrupt(QJS_Runtime *rt) {
  if (!rt)
    return;
  RuntimeState *state = get_runtime_state((JSRuntime *)rt);
  if (state) {
    state->interrupted = 1;
  }
}

void qjs_clear_interrupt(QJS_Runtime *rt) {
  if (!rt)
    return;
  RuntimeState *state = get_runtime_state((JSRuntime *)rt);
  if (state) {
    state->interrupted = 0;
    state->instruction_count = 0;
  }
}

/* ========== Public API: Context ========== */

QJS_Context *qjs_new_context(QJS_Runtime *rt) {
  if (!rt)
    return NULL;

  /* JS_NewContext already adds all standard intrinsics:
   * BaseObjects, Date, Eval, StringNormalize, RegExp, JSON,
   * Proxy, MapSet, TypedArrays, Promise, WeakRef
   */
  JSContext *ctx = JS_NewContext((JSRuntime *)rt);
  if (!ctx)
    return NULL;

  /* Allocate per-context state */
  ContextState *state = (ContextState *)calloc(1, sizeof(ContextState));
  if (!state) {
    JS_FreeContext(ctx);
    return NULL;
  }
  state->next_promise_id = 1;

  JS_SetContextOpaque(ctx, state);
  return (QJS_Context *)ctx;
}

void qjs_free_context(QJS_Context *ctx) {
  if (!ctx)
    return;

  ContextState *state = get_context_state((JSContext *)ctx);
  free(state); /* Free per-context state (callbacks auto-released) */

  JS_FreeContext((JSContext *)ctx);
}

char *qjs_eval(QJS_Context *ctx, const char *code, size_t len,
               const char *filename) {
  if (!ctx || !code)
    return NULL;

  JSContext *js_ctx = (JSContext *)ctx;
  /* Note: We don't use JS_EVAL_FLAG_ASYNC because we handle async through
   * our pending ops mechanism (toolCall, fetch, etc.) rather than through
   * QuickJS's built-in async evaluation. */
  JSValue result = JS_Eval(js_ctx, code, len, filename ? filename : "<eval>",
                           JS_EVAL_TYPE_GLOBAL);

  if (JS_IsException(result)) {
    return NULL; /* Caller should call qjs_get_exception */
  }

  char *json = js_value_to_json(js_ctx, result);
  JS_FreeValue(js_ctx, result);
  return json;
}

void qjs_free_string(char *str) { free(str); }

char *qjs_get_exception(QJS_Context *ctx) {
  if (!ctx)
    return NULL;
  return get_exception_json((JSContext *)ctx);
}

void qjs_clear_exception(QJS_Context *ctx) {
  if (!ctx)
    return;
  JSContext *js_ctx = (JSContext *)ctx;
  /* Getting the exception clears it */
  JSValue exc = JS_GetException(js_ctx);
  JS_FreeValue(js_ctx, exc);
}

int qjs_set_global_json(QJS_Context *ctx, const char *name, const char *json) {
  if (!ctx || !name)
    return QJS_ERROR;

  JSContext *js_ctx = (JSContext *)ctx;
  JSValue global = JS_GetGlobalObject(js_ctx);
  JSValue val = json ? json_to_js_value(js_ctx, json) : JS_UNDEFINED;

  if (JS_IsException(val)) {
    JS_FreeValue(js_ctx, global);
    return QJS_EXCEPTION;
  }

  int ret = JS_SetPropertyStr(js_ctx, global, name, val);
  JS_FreeValue(js_ctx, global);
  return ret < 0 ? QJS_ERROR : QJS_OK;
}

char *qjs_get_global_json(QJS_Context *ctx, const char *name) {
  if (!ctx || !name)
    return NULL;

  JSContext *js_ctx = (JSContext *)ctx;
  JSValue global = JS_GetGlobalObject(js_ctx);
  JSValue val = JS_GetPropertyStr(js_ctx, global, name);
  JS_FreeValue(js_ctx, global);

  if (JS_IsException(val)) {
    return NULL;
  }

  char *json = js_value_to_json(js_ctx, val);
  JS_FreeValue(js_ctx, val);
  return json;
}

int qjs_add_function(QJS_Context *ctx, const char *name, QJS_CCallback callback,
                     void *user_data) {
  if (!ctx || !name || !callback)
    return QJS_ERROR;

  JSContext *js_ctx = (JSContext *)ctx;
  JSValue global = JS_GetGlobalObject(js_ctx);

  /* Allocate a callback slot in this context's registry */
  int slot_id = allocate_callback_slot(js_ctx, callback, user_data);
  if (slot_id < 0) {
    JS_FreeValue(js_ctx, global);
    return QJS_ERROR; /* Registry full */
  }

  JSValue func_data[1];
  func_data[0] = JS_NewInt32(js_ctx, slot_id);

  JSValue func =
      JS_NewCFunctionData(js_ctx, js_callback_trampoline, 0, 0, 1, func_data);
  /* JS_NewCFunctionData dups func_data values, so free our local copy */
  JS_FreeValue(js_ctx, func_data[0]);

  if (JS_IsException(func)) {
    /* Note: Can't easily "free" the slot, but context cleanup handles it */
    JS_FreeValue(js_ctx, global);
    return QJS_EXCEPTION;
  }

  int ret = JS_SetPropertyStr(js_ctx, global, name, func);
  JS_FreeValue(js_ctx, global);
  return ret < 0 ? QJS_ERROR : QJS_OK;
}

char *qjs_call_function(QJS_Context *ctx, const char *name,
                        const char *args_json) {
  if (!ctx || !name)
    return NULL;

  JSContext *js_ctx = (JSContext *)ctx;
  JSValue global = JS_GetGlobalObject(js_ctx);
  JSValue func = JS_GetPropertyStr(js_ctx, global, name);

  if (!JS_IsFunction(js_ctx, func)) {
    JS_FreeValue(js_ctx, func);
    JS_FreeValue(js_ctx, global);
    return NULL;
  }

  /* Parse args */
  JSValue args =
      args_json ? json_to_js_value(js_ctx, args_json) : JS_NewArray(js_ctx);
  if (JS_IsException(args)) {
    JS_FreeValue(js_ctx, func);
    JS_FreeValue(js_ctx, global);
    return NULL;
  }

  /* Build argv from args array */
  int argc = 0;
  JSValue *argv = NULL;

  if (JS_IsArray(js_ctx, args)) {
    JSValue len_val = JS_GetPropertyStr(js_ctx, args, "length");
    JS_ToInt32(js_ctx, &argc, len_val);
    JS_FreeValue(js_ctx, len_val);

    if (argc > 0) {
      argv = (JSValue *)malloc(argc * sizeof(JSValue));
      if (argv) {
        for (int i = 0; i < argc; i++) {
          argv[i] = JS_GetPropertyUint32(js_ctx, args, i);
        }
      }
    }
  }

  JSValue result = JS_Call(js_ctx, func, global, argc, argv);

  /* Cleanup argv */
  if (argv) {
    for (int i = 0; i < argc; i++) {
      JS_FreeValue(js_ctx, argv[i]);
    }
    free(argv);
  }
  JS_FreeValue(js_ctx, args);
  JS_FreeValue(js_ctx, func);
  JS_FreeValue(js_ctx, global);

  if (JS_IsException(result)) {
    return NULL;
  }

  char *json = js_value_to_json(js_ctx, result);
  JS_FreeValue(js_ctx, result);
  return json;
}

/* Default max iterations to prevent infinite microtask loops.
 * This is a safety limit for qjs_run_pending_jobs. */
#define QJS_DEFAULT_MAX_ITERATIONS 10000

int qjs_run_pending_jobs(QJS_Runtime *rt) {
  return qjs_run_pending_jobs_limited(rt, QJS_DEFAULT_MAX_ITERATIONS);
}

int qjs_run_pending_jobs_limited(QJS_Runtime *rt, int max_iterations) {
  if (!rt)
    return QJS_ERROR;
  if (max_iterations <= 0)
    max_iterations = QJS_DEFAULT_MAX_ITERATIONS;

  JSRuntime *js_rt = (JSRuntime *)rt;
  JSContext *pctx;
  int ret;
  int executed = 0;

  while (JS_IsJobPending(js_rt)) {
    /* Check iteration budget to prevent infinite microtask loops */
    if (executed >= max_iterations) {
      return QJS_BUDGET_EXHAUSTED;
    }

    ret = JS_ExecutePendingJob(js_rt, &pctx);
    if (ret < 0) {
      /* Exception in job */
      return QJS_EXCEPTION;
    }
    if (ret == 0) {
      break;
    }
    executed++;
  }

  return executed;
}

int qjs_has_pending_jobs(QJS_Runtime *rt) {
  if (!rt)
    return 0;
  return JS_IsJobPending((JSRuntime *)rt);
}

/* User opaque API - forwards to our wrapper struct */
void qjs_set_context_opaque(QJS_Context *ctx, void *opaque) {
  if (!ctx)
    return;
  ContextState *state = get_context_state((JSContext *)ctx);
  if (state) {
    state->user_opaque = opaque;
  }
}

void *qjs_get_context_opaque(QJS_Context *ctx) {
  if (!ctx)
    return NULL;
  ContextState *state = get_context_state((JSContext *)ctx);
  return state ? state->user_opaque : NULL;
}

/* ========== Promise Manipulation ========== */

char *qjs_new_promise(QJS_Context *ctx, uint64_t *promise_id) {
  if (!ctx)
    return NULL;

  JSContext *js_ctx = (JSContext *)ctx;
  ContextState *state = get_context_state(js_ctx);
  if (!state)
    return NULL;

  JSValue resolving_funcs[2];
  JSValue promise = JS_NewPromiseCapability(js_ctx, resolving_funcs);

  if (JS_IsException(promise)) {
    return NULL;
  }

  /* Use per-context promise ID counter */
  uint64_t id = state->next_promise_id++;
  *promise_id = id;

  char resolve_name[32], reject_name[32];
  snprintf(resolve_name, sizeof(resolve_name), "__resolve_%llu",
           (unsigned long long)id);
  snprintf(reject_name, sizeof(reject_name), "__reject_%llu",
           (unsigned long long)id);

  JSValue global = JS_GetGlobalObject(js_ctx);
  JS_SetPropertyStr(js_ctx, global, resolve_name, resolving_funcs[0]);
  JS_SetPropertyStr(js_ctx, global, reject_name, resolving_funcs[1]);
  JS_FreeValue(js_ctx, global);

  char *json = js_value_to_json(js_ctx, promise);
  JS_FreeValue(js_ctx, promise);
  return json;
}

int qjs_resolve_promise(QJS_Context *ctx, uint64_t promise_id,
                        const char *value_json) {
  if (!ctx)
    return QJS_ERROR;

  JSContext *js_ctx = (JSContext *)ctx;
  char resolve_name[32];
  snprintf(resolve_name, sizeof(resolve_name), "__resolve_%llu",
           (unsigned long long)promise_id);

  JSValue global = JS_GetGlobalObject(js_ctx);
  JSValue resolve_fn = JS_GetPropertyStr(js_ctx, global, resolve_name);

  if (!JS_IsFunction(js_ctx, resolve_fn)) {
    JS_FreeValue(js_ctx, resolve_fn);
    JS_FreeValue(js_ctx, global);
    return QJS_ERROR;
  }

  JSValue value =
      value_json ? json_to_js_value(js_ctx, value_json) : JS_UNDEFINED;
  JSValue result = JS_Call(js_ctx, resolve_fn, JS_UNDEFINED, 1, &value);
  JS_FreeValue(js_ctx, value);
  JS_FreeValue(js_ctx, resolve_fn);

  /* Cleanup stored functions */
  char reject_name[32];
  snprintf(reject_name, sizeof(reject_name), "__reject_%llu",
           (unsigned long long)promise_id);
  JS_SetPropertyStr(js_ctx, global, resolve_name, JS_UNDEFINED);
  JS_SetPropertyStr(js_ctx, global, reject_name, JS_UNDEFINED);
  JS_FreeValue(js_ctx, global);

  int ret = JS_IsException(result) ? QJS_EXCEPTION : QJS_OK;
  JS_FreeValue(js_ctx, result);
  return ret;
}

int qjs_reject_promise(QJS_Context *ctx, uint64_t promise_id,
                       const char *error_json) {
  if (!ctx)
    return QJS_ERROR;

  JSContext *js_ctx = (JSContext *)ctx;
  char reject_name[32];
  snprintf(reject_name, sizeof(reject_name), "__reject_%llu",
           (unsigned long long)promise_id);

  JSValue global = JS_GetGlobalObject(js_ctx);
  JSValue reject_fn = JS_GetPropertyStr(js_ctx, global, reject_name);

  if (!JS_IsFunction(js_ctx, reject_fn)) {
    JS_FreeValue(js_ctx, reject_fn);
    JS_FreeValue(js_ctx, global);
    return QJS_ERROR;
  }

  JSValue error =
      error_json ? json_to_js_value(js_ctx, error_json) : JS_UNDEFINED;
  JSValue result = JS_Call(js_ctx, reject_fn, JS_UNDEFINED, 1, &error);
  JS_FreeValue(js_ctx, error);
  JS_FreeValue(js_ctx, reject_fn);

  /* Cleanup stored functions */
  char resolve_name[32];
  snprintf(resolve_name, sizeof(resolve_name), "__resolve_%llu",
           (unsigned long long)promise_id);
  JS_SetPropertyStr(js_ctx, global, resolve_name, JS_UNDEFINED);
  JS_SetPropertyStr(js_ctx, global, reject_name, JS_UNDEFINED);
  JS_FreeValue(js_ctx, global);

  int ret = JS_IsException(result) ? QJS_EXCEPTION : QJS_OK;
  JS_FreeValue(js_ctx, result);
  return ret;
}
