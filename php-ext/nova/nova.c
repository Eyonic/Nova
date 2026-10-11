/*
 * NOVA worker mode, native: lets a long-running PHP CLI worker serve HTTP
 * requests for NOVA with PHP's own request machinery.
 *
 *   while (nova_handle_request(function () { ...serve one request... })) {}
 *
 * (also available as frankenphp_handle_request(), so Laravel Octane's and
 * Symfony Runtime's FrankenPHP workers run unchanged.)
 *
 * Each call reads one request from NOVA (goridge frames on stdin: the CGI
 * parameters NOVA would give PHP-FPM, then the raw body), and runs the
 * callback inside a per-request PHP state built the way a SAPI builds it:
 * $_SERVER from the parameters, $_GET/$_POST/$_COOKIE/$_FILES parsed by PHP
 * itself (multipart uploads included, removed after the request),
 * php://input, header() and http_response_code(). Output and headers go
 * back to NOVA as an FPM-style CGI response (frames on stdout). The
 * per-request reset follows FrankenPHP's worker mode (frankenphp.c, MIT).
 *
 * Outside a request, output goes to stderr: stdout carries only frames.
 */

#ifdef HAVE_CONFIG_H
#include "config.h"
#endif

#include "php.h"
#include "SAPI.h"
#include "php_main.h"
#include "php_variables.h"
#include "ext/standard/info.h"
#include "ext/standard/head.h"
#include "main/php_output.h"
#include "zend_smart_str.h"
#include "zend_exceptions.h"
#include <unistd.h>
#include <errno.h>
#include <string.h>

#define NOVA_VERSION "1"

/* goridge (see nova-runtime-php goridge.rs) */
#define G_CONTROL 0x01
#define G_STREAM 0x01
#define NOVA_FASTCGI 0x02   /* byte11: request params are FastCGI-encoded */
#define NOVA_CGI_HEAD 0x02  /* byte11: response header is a CGI block */

static uint32_t crc_table[256];

static void crc_init(void)
{
	for (uint32_t i = 0; i < 256; i++) {
		uint32_t c = i;
		for (int k = 0; k < 8; k++) {
			c = c & 1 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
		}
		crc_table[i] = c;
	}
}

static uint32_t crc32_6(const unsigned char *p)
{
	uint32_t c = 0xFFFFFFFFu;
	for (int i = 0; i < 6; i++) {
		c = crc_table[(c ^ p[i]) & 0xFF] ^ (c >> 8);
	}
	return c ^ 0xFFFFFFFFu;
}

static int read_full(int fd, void *buf, size_t n)
{
	char *p = buf;
	while (n > 0) {
		ssize_t r = read(fd, p, n);
		if (r == 0) {
			return -1;
		}
		if (r < 0) {
			if (errno == EINTR) {
				continue;
			}
			return -1;
		}
		p += r;
		n -= (size_t)r;
	}
	return 0;
}

static int write_full(int fd, const void *buf, size_t n)
{
	const char *p = buf;
	while (n > 0) {
		ssize_t w = write(fd, p, n);
		if (w < 0) {
			if (errno == EINTR) {
				continue;
			}
			return -1;
		}
		p += w;
		n -= (size_t)w;
	}
	return 0;
}

/* ---- the current request ---- */

typedef struct {
	bool active;
	/* request */
	char *payload;      /* params + body (owned) */
	size_t params_len;
	char *body;
	size_t body_len;
	size_t body_read;
	char *cookie;       /* HTTP_COOKIE value (in payload) */
	/* response */
	smart_str out;      /* buffered output */
	smart_str head;     /* CGI header block, once headers are sent */
	bool head_sent;     /* first frame already written */
	bool streaming;
} nova_request;

static nova_request req;
static bool worker_enabled;
static bool startup_closed;

/* The CLI's own callbacks, used outside of requests. */
static sapi_module_struct cli_module;

static void send_frame(const char *head, size_t head_len, const char *body, size_t body_len, bool more)
{
	unsigned char h[16];
	size_t payload = head_len + body_len;
	h[0] = (1 << 4) | 4; /* version 1, 4 header words (one option) */
	h[1] = 0x08;         /* CODEC_JSON (ignored) */
	h[2] = payload & 0xFF;
	h[3] = (payload >> 8) & 0xFF;
	h[4] = (payload >> 16) & 0xFF;
	h[5] = (payload >> 24) & 0xFF;
	uint32_t crc = crc32_6(h);
	h[6] = crc & 0xFF;
	h[7] = (crc >> 8) & 0xFF;
	h[8] = (crc >> 16) & 0xFF;
	h[9] = (crc >> 24) & 0xFF;
	h[10] = more ? G_STREAM : 0;
	h[11] = NOVA_CGI_HEAD;
	h[12] = head_len & 0xFF;
	h[13] = (head_len >> 8) & 0xFF;
	h[14] = (head_len >> 16) & 0xFF;
	h[15] = (head_len >> 24) & 0xFF;
	if (write_full(STDOUT_FILENO, h, sizeof h) < 0
		|| (head_len && write_full(STDOUT_FILENO, head, head_len) < 0)
		|| (body_len && write_full(STDOUT_FILENO, body, body_len) < 0)) {
		/* NOVA is gone: stop at the next opportunity. */
		PG(connection_status) = PHP_CONNECTION_ABORTED;
	}
}

/* Build the CGI header block from PHP's response headers. */
static int nova_send_headers(sapi_headers_struct *sapi_headers)
{
	if (!req.active) {
		return SAPI_HEADER_SENT_SUCCESSFULLY;
	}
	int status = SG(sapi_headers).http_response_code;
	if (!status) {
		status = 200;
	}
	smart_str_free(&req.head);
	smart_str_appends(&req.head, "Status: ");
	smart_str_append_long(&req.head, status);
	smart_str_appends(&req.head, "\r\n");
	zend_llist_position pos;
	sapi_header_struct *h = zend_llist_get_first_ex(&sapi_headers->headers, &pos);
	while (h) {
		smart_str_appendl(&req.head, h->header, h->header_len);
		smart_str_appends(&req.head, "\r\n");
		h = zend_llist_get_next_ex(&sapi_headers->headers, &pos);
	}
	smart_str_appends(&req.head, "\r\n");
	smart_str_0(&req.head);
	return SAPI_HEADER_SENT_SUCCESSFULLY;
}

static size_t nova_ub_write(const char *str, size_t len)
{
	if (!req.active) {
		/* Never on stdout: that is the frame channel. */
		write_full(STDERR_FILENO, str, len);
		return len;
	}
	smart_str_appendl(&req.out, str, len);
	return len;
}

/* flush(): stream what is buffered (server-sent events, progress...). */
static void nova_flush(void *server_context)
{
	if (!req.active) {
		return;
	}
	if (!SG(headers_sent)) {
		sapi_send_headers();
	}
	const char *head = req.head_sent ? "" : (req.head.s ? ZSTR_VAL(req.head.s) : "");
	size_t head_len = req.head_sent ? 0 : (req.head.s ? ZSTR_LEN(req.head.s) : 0);
	size_t out_len = req.out.s ? ZSTR_LEN(req.out.s) : 0;
	if (out_len == 0 && req.head_sent) {
		return;
	}
	send_frame(head, head_len, out_len ? ZSTR_VAL(req.out.s) : "", out_len, true);
	req.head_sent = true;
	req.streaming = true;
	if (req.out.s) {
		ZSTR_LEN(req.out.s) = 0;
	}
}

static size_t nova_read_post(char *buf, size_t n)
{
	if (!req.active) {
		return 0;
	}
	size_t left = req.body_len - req.body_read;
	if (n > left) {
		n = left;
	}
	memcpy(buf, req.body + req.body_read, n);
	req.body_read += n;
	return n;
}

static char *nova_read_cookies(void)
{
	return req.active ? req.cookie : NULL;
}

/* FastCGI name-value pair length (1 or 4 bytes). */
static int fcgi_len(const unsigned char **p, const unsigned char *end, size_t *out)
{
	if (*p >= end) {
		return -1;
	}
	if (**p < 0x80) {
		*out = *(*p)++;
		return 0;
	}
	if (end - *p < 4) {
		return -1;
	}
	*out = ((size_t)((*p)[0] & 0x7F) << 24) | ((size_t)(*p)[1] << 16) | ((size_t)(*p)[2] << 8) | (*p)[3];
	*p += 4;
	return 0;
}

/* Walk the request parameters: fn(name, name_len, value, value_len, ctx). */
static void each_param(void (*fn)(const char *, size_t, char *, size_t, void *), void *ctx)
{
	const unsigned char *p = (const unsigned char *)req.payload;
	const unsigned char *end = p + req.params_len;
	while (p < end) {
		size_t nl, vl;
		if (fcgi_len(&p, end, &nl) || fcgi_len(&p, end, &vl) || (size_t)(end - p) < nl + vl) {
			return;
		}
		fn((const char *)p, nl, (char *)p + nl, vl, ctx);
		p += nl + vl;
	}
}

static void register_one(const char *name, size_t nl, char *val, size_t vl, void *ctx)
{
	char key[256];
	if (nl == 0 || nl >= sizeof key) {
		return;
	}
	memcpy(key, name, nl);
	key[nl] = 0;
	size_t new_len = vl;
	char *v = val;
	if (sapi_module.input_filter(PARSE_SERVER, key, &v, vl, &new_len)) {
		php_register_variable_safe(key, v, new_len, (zval *)ctx);
	}
}

static void nova_register_variables(zval *track_vars_array)
{
	if (!req.active) {
		cli_module.register_server_variables(track_vars_array);
		return;
	}
	/* Like FPM with clear_env = no: the worker's environment, then the request. */
	php_import_environment_variables(track_vars_array);
	each_param(register_one, track_vars_array);
}

/* Point SG(request_info) at the request (strings live in req.payload). */
typedef struct {
	char *method, *query, *uri, *ctype, *script;
	size_t clen;
} info_ctx;

static char *dupz(char *v, size_t vl)
{
	/* Values are not NUL-terminated in the payload. */
	return estrndup(v, vl);
}

static void collect(const char *name, size_t nl, char *val, size_t vl, void *ctx)
{
	info_ctx *i = ctx;
#define IS(s) (nl == sizeof(s) - 1 && memcmp(name, s, nl) == 0)
	if (IS("REQUEST_METHOD")) {
		i->method = dupz(val, vl);
	} else if (IS("QUERY_STRING")) {
		i->query = dupz(val, vl);
	} else if (IS("REQUEST_URI")) {
		i->uri = dupz(val, vl);
	} else if (IS("CONTENT_TYPE")) {
		i->ctype = dupz(val, vl);
	} else if (IS("SCRIPT_FILENAME")) {
		i->script = dupz(val, vl);
	} else if (IS("CONTENT_LENGTH")) {
		char tmp[32];
		size_t n = vl < sizeof tmp - 1 ? vl : sizeof tmp - 1;
		memcpy(tmp, val, n);
		tmp[n] = 0;
		i->clen = (size_t)strtoull(tmp, NULL, 10);
	} else if (IS("HTTP_COOKIE")) {
		req.cookie = dupz(val, vl);
	}
#undef IS
}

static info_ctx info;

/* Tell the manager this worker waits for a request: it hands requests
 * only to workers that asked, so none goes to a worker that is leaving
 * its loop (max requests, exit). */
static void send_ready(void)
{
	static const char json[] = "{\"ready\":true}";
	unsigned char f[12 + sizeof json - 1];
	size_t len = sizeof json - 1;
	f[0] = (1 << 4) | 3;
	f[1] = G_CONTROL;
	f[2] = len & 0xFF;
	f[3] = (len >> 8) & 0xFF;
	f[4] = 0;
	f[5] = 0;
	uint32_t crc = crc32_6(f);
	f[6] = crc & 0xFF;
	f[7] = (crc >> 8) & 0xFF;
	f[8] = (crc >> 16) & 0xFF;
	f[9] = (crc >> 24) & 0xFF;
	f[10] = 0;
	f[11] = 0;
	memcpy(f + 12, json, len);
	(void)write_full(STDOUT_FILENO, f, sizeof f);
}

/* Read the next request. 0: got one, 1: stop (control frame or EOF). */
static int next_request(void)
{
	send_ready();
	for (;;) {
		unsigned char h[12];
		if (read_full(STDIN_FILENO, h, sizeof h) < 0) {
			return 1;
		}
		size_t words = h[0] & 0x0F;
		size_t len = h[2] | (h[3] << 8) | (h[4] << 16) | ((size_t)h[5] << 24);
		size_t opt_len = words > 3 ? (words - 3) * 4 : 0;
		unsigned char opts[64];
		if (opt_len > sizeof opts || read_full(STDIN_FILENO, opts, opt_len) < 0) {
			return 1;
		}
		char *payload = emalloc(len + 1);
		if (read_full(STDIN_FILENO, payload, len) < 0) {
			efree(payload);
			return 1;
		}
		payload[len] = 0;
		if (h[1] & G_CONTROL) {
			bool stop = strstr(payload, "stop") != NULL;
			efree(payload);
			if (stop) {
				return 1;
			}
			continue;
		}
		size_t params_len = opt_len >= 4 ? (opts[0] | (opts[1] << 8) | (opts[2] << 16) | ((size_t)opts[3] << 24)) : 0;
		if (params_len > len || !(h[11] & NOVA_FASTCGI)) {
			efree(payload);
			continue;
		}
		req.payload = payload;
		req.params_len = params_len;
		req.body = payload + params_len;
		req.body_len = len - params_len;
		req.body_read = 0;
		return 0;
	}
}

/* Adapted from FrankenPHP's frankenphp_reset_super_globals(). */
static void reset_super_globals(void)
{
	zend_try {
		zval *files = &PG(http_globals)[TRACK_VARS_FILES];
		zval_ptr_dtor_nogc(files);
		memset(files, 0, sizeof(*files));
		zend_hash_str_del(&EG(symbol_table), "_SESSION", sizeof("_SESSION") - 1);
	}
	zend_end_try();

	zend_auto_global *ag;
	zend_string *env = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_ENV);
	zend_string *server = ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_SERVER);
	ZEND_HASH_MAP_FOREACH_PTR(CG(auto_globals), ag) {
		if (ag->name == env) {
			continue;
		} else if (ag->name == server) {
			ag->armed = ag->auto_global_callback(ag->name);
		} else if (ag->jit) {
			if (ag->name == ZSTR_KNOWN(ZEND_STR_AUTOGLOBAL_REQUEST)
				&& zend_hash_exists(&EG(symbol_table), ag->name)) {
				ag->armed = ag->auto_global_callback(ag->name);
			}
		} else if (ag->auto_global_callback) {
			ag->armed = ag->auto_global_callback(ag->name);
		} else {
			ag->armed = 0;
		}
	}
	ZEND_HASH_FOREACH_END();
}

static void reload_module(const char *name, bool startup)
{
	zend_module_entry *m = zend_hash_str_find_ptr(&module_registry, name, strlen(name));
	if (!m) {
		return;
	}
	if (startup && m->request_startup_func) {
		m->request_startup_func(m->type, m->module_number);
	} else if (!startup && m->request_shutdown_func) {
		m->request_shutdown_func(m->type, m->module_number);
	}
}

static const char *modules_to_reload[] = {"filter", "session", NULL};

/* Adapted from FrankenPHP's frankenphp_worker_request_startup(). */
static int request_startup(void)
{
	int ok = SUCCESS;
	memset(&info, 0, sizeof info);
	req.cookie = NULL;
	each_param(collect, &info);

	SG(server_context) = (void *)&req; /* non-NULL: SAPI reads POST data and cookies */
	SG(request_info).request_method = info.method;
	SG(request_info).query_string = info.query;
	SG(request_info).request_uri = info.uri;
	SG(request_info).content_type = info.ctype;
	SG(request_info).content_length = (zend_long)info.clen;
	SG(request_info).path_translated = info.script;
	SG(request_info).proto_num = 1001;

	req.active = true;
	req.head_sent = false;
	req.streaming = false;
	smart_str_free(&req.out);
	smart_str_free(&req.head);

	zend_try {
		php_output_activate();
		PG(header_is_being_sent) = 0;
		PG(connection_status) = PHP_CONNECTION_NORMAL;
		sapi_activate();
		SG(request_info).no_headers = 0;
		if (PG(expose_php)) {
			sapi_add_header(SAPI_PHP_VERSION_HEADER, sizeof(SAPI_PHP_VERSION_HEADER) - 1, 1);
		}
		if (PG(output_buffering)) {
			php_output_start_user(NULL, PG(output_buffering) > 1 ? PG(output_buffering) : 0,
				PHP_OUTPUT_HANDLER_STDFLAGS);
		}
		/* The CLI forces implicit_flush on: every echo would become its own
		 * frame. Explicit flush() still streams. */
		php_output_set_implicit_flush(0);
		reset_super_globals();
		for (const char **m = modules_to_reload; *m; m++) {
			reload_module(*m, true);
		}
	}
	zend_catch {
		ok = FAILURE;
	}
	zend_end_try();
	SG(sapi_started) = 1;
	return ok;
}

/* Adapted from FrankenPHP's frankenphp_worker_request_shutdown(). */
static void request_shutdown(void)
{
	zend_try {
		php_output_end_all();
	}
	zend_end_try();
	zend_try {
		if (!SG(headers_sent)) {
			php_header();
		}
	}
	zend_end_try();

	/* The response: whatever was not streamed yet, as the last frame. */
	const char *head = req.head_sent ? "" : (req.head.s ? ZSTR_VAL(req.head.s) : "Status: 200\r\n\r\n");
	size_t head_len = req.head_sent ? 0 : (req.head.s ? ZSTR_LEN(req.head.s) : strlen(head));
	size_t out_len = req.out.s ? ZSTR_LEN(req.out.s) : 0;
	send_frame(head, head_len, out_len ? ZSTR_VAL(req.out.s) : "", out_len, false);

	for (const char **m = modules_to_reload; *m; m++) {
		reload_module(*m, false);
	}
	zend_try {
		php_output_deactivate();
	}
	zend_end_try();
	zend_try {
		sapi_deactivate(); /* also removes uploaded files */
	}
	zend_end_try();

	req.active = false;
	SG(server_context) = NULL;
	if (info.method) efree(info.method);
	if (info.query) efree(info.query);
	if (info.uri) efree(info.uri);
	if (info.ctype) efree(info.ctype);
	if (info.script) efree(info.script);
	if (req.cookie) efree(req.cookie);
	memset(&info, 0, sizeof info);
	req.cookie = NULL;
	SG(request_info).request_method = NULL;
	SG(request_info).query_string = NULL;
	SG(request_info).request_uri = NULL;
	SG(request_info).content_type = NULL;
	SG(request_info).path_translated = NULL;
	efree(req.payload);
	req.payload = NULL;
	smart_str_free(&req.out);
	smart_str_free(&req.head);
	zend_set_memory_limit(PG(memory_limit));
}

/* {{{ nova_handle_request(callable $handler): bool */
PHP_FUNCTION(nova_handle_request)
{
	zend_fcall_info fci;
	zend_fcall_info_cache fcc;

	ZEND_PARSE_PARAMETERS_START(1, 1)
		Z_PARAM_FUNC(fci, fcc)
	ZEND_PARSE_PARAMETERS_END();

	if (!worker_enabled) {
		zend_throw_exception(zend_ce_exception, "nova_handle_request() needs NOVA worker mode", 0);
		RETURN_THROWS();
	}
	if (!startup_closed) {
		/* The worker script runs inside the CLI's own request: close its
		 * output and SAPI state once, each request then gets its own. */
		zend_try {
			php_output_end_all();
			php_output_deactivate();
			sapi_deactivate();
		}
		zend_end_try();
		startup_closed = true;
	}
	if (next_request() != 0) {
		/* Done: give the rest of the script a normal state to finish in. */
		SG(server_context) = NULL;
		php_output_activate();
		sapi_activate();
		SG(request_info).no_headers = 1;
		RETURN_FALSE;
	}
	if (request_startup() == FAILURE) {
		request_shutdown();
		RETURN_FALSE;
	}

	zval retval;
	ZVAL_UNDEF(&retval);
	fci.size = sizeof fci;
	fci.retval = &retval;
	fci.params = NULL;
	fci.param_count = 0;
	zend_call_function(&fci, &fcc);
	if (EG(exception)) {
		if (!zend_is_unwind_exit(EG(exception)) && !zend_is_graceful_exit(EG(exception))) {
			zend_exception_error(EG(exception), E_ERROR);
		} else {
			request_shutdown();
			zend_bailout();
		}
	}
	zval_ptr_dtor(&retval);
	request_shutdown();
	RETURN_TRUE;
}
/* }}} */

ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_nova_handle_request, 0, 1, _IS_BOOL, 0)
	ZEND_ARG_TYPE_INFO(0, handler, IS_CALLABLE, 0)
ZEND_END_ARG_INFO()

static const zend_function_entry nova_functions[] = {
	PHP_FE(nova_handle_request, arginfo_nova_handle_request)
	PHP_FALIAS(frankenphp_handle_request, nova_handle_request, arginfo_nova_handle_request)
	PHP_FE_END
};

PHP_MINIT_FUNCTION(nova)
{
	const char *on = getenv("NOVA_WORKER");
	worker_enabled = on && strcmp(on, "1") == 0 && strcmp(sapi_module.name, "cli") == 0;
	if (!worker_enabled) {
		return SUCCESS;
	}
	crc_init();
	cli_module = sapi_module;
	sapi_module.ub_write = nova_ub_write;
	sapi_module.flush = nova_flush;
	sapi_module.send_headers = nova_send_headers;
	sapi_module.send_header = NULL;
	sapi_module.header_handler = NULL; /* keep headers (the CLI drops them) */
	sapi_module.read_post = nova_read_post;
	sapi_module.read_cookies = nova_read_cookies;
	sapi_module.register_server_variables = nova_register_variables;
	return SUCCESS;
}

PHP_MINFO_FUNCTION(nova)
{
	php_info_print_table_start();
	php_info_print_table_row(2, "NOVA worker mode", worker_enabled ? "active" : "inactive");
	php_info_print_table_row(2, "Protocol", NOVA_VERSION);
	php_info_print_table_end();
}

zend_module_entry nova_module_entry = {
	STANDARD_MODULE_HEADER,
	"nova",
	nova_functions,
	PHP_MINIT(nova),
	NULL,
	NULL,
	NULL,
	PHP_MINFO(nova),
	NOVA_VERSION,
	STANDARD_MODULE_PROPERTIES
};

#ifdef COMPILE_DL_NOVA
ZEND_GET_MODULE(nova)
#endif
