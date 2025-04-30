/*
 * This code is part of Qiskit.
 *
 * (C) Copyright IBM 2025.
 *
 * This code is licensed under the Apache License, Version 2.0. You may
 * obtain a copy of this license in the LICENSE.txt file in the root directory
 * of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
 *
 * Any modifications or derivative works of this code must retain this
 * copyright notice, and modified files need to carry a notice indicating
 * that they have been altered from the originals.
 */
#include <ctype.h>
#include <grp.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdio.h>
#include <sys/stat.h>

#include "slurm/slurm.h"
#include "slurm/spank.h"
#include "qrmi.h"

SPANK_PLUGIN(spank_ibm_qrmi, 1)

#define MAXLEN_BACKEND_NAME 256
#define MAXLEN_PROGRAM_ID   256
#define MAXLEN_JOB_ID      1024
#define MAX_JSON_FILE     65536

/* Globals for options */
static char backend_name [MAXLEN_BACKEND_NAME + 1];
static char primitive_type[MAXLEN_PROGRAM_ID  + 1];
static char qrun_job_id   [MAXLEN_JOB_ID      + 1];
static char qrmi_conf_path[PATH_MAX];
static char token[1024];

/* Linked list to store conf key/value pairs */
typedef struct kv_node {
    char *key;
    char *val;
    struct kv_node *next;
} kv_node_t;
static kv_node_t *conf_list = NULL;

/* Helper to free existing list (if reloading) */
static void free_conf_list(void) {
    kv_node_t *node = conf_list;
    while (node) {
        kv_node_t *next = node->next;
        free(node->key);
        free(node->val);
        free(node);
        node = next;
    }
    conf_list = NULL;
}

/* Simple safe strncpy */
static char *strncpy_s(char *dst, const char *src, size_t dsize) {
    if (dsize == 0) return dst;
    strncpy(dst, src, dsize - 1);
    dst[dsize - 1] = '\0';
    return dst;
}

/* Skip whitespace */
static const char *skip_ws(const char *p) {
    while (*p && isspace((unsigned char)*p)) p++;
    return p;
}

/* Parse "key": value (quoted or bare) */
static const char *parse_kv(const char *p, char **key, char **val) {
    p = skip_ws(p);
    if (*p != '"') return NULL;
    const char *kstart = ++p;
    const char *kend   = strchr(kstart, '"');
    if (!kend) return NULL;
    size_t klen = kend - kstart;
    *key = malloc(klen + 1);
    memcpy(*key, kstart, klen);
    (*key)[klen] = '\0';

    p = skip_ws(kend + 1);
    if (*p != ':') { free(*key); return NULL; }
    p = skip_ws(p + 1);

    const char *vstart = p;
    char *out;
    if (*p == '"') {
        vstart = ++p;
        const char *vend = strchr(vstart, '"');
        if (!vend) { free(*key); return NULL; }
        size_t vlen = vend - vstart;
        out = malloc(vlen + 1);
        memcpy(out, vstart, vlen);
        out[vlen] = '\0';
        p = vend + 1;
    } else {
        while (*p && *p != ',' && *p != '}' && !isspace((unsigned char)*p)) p++;
        size_t vlen = p - vstart;
        out = malloc(vlen + 1);
        memcpy(out, vstart, vlen);
        out[vlen] = '\0';
    }
    *val = out;
    p = skip_ws(p);
    if (*p == ',') p++;
    return p;
}

static int backend_name_cb(int val, const char *optarg, int remote)
{
    slurm_debug("hogehogeho");
    slurm_debug("%s: %s val=%d optarg=%s remote=%d",
            plugin_name, __FUNCTION__, val, optarg, remote);
    strncpy_s(backend_name, optarg, sizeof(backend_name));
    return ESPANK_SUCCESS;
}

/*
 * @function primitive_type_cb
 *
 * Callback function to check --q-primitive option value.
 *
 */
static int primitive_type_cb(int val, const char *optarg, int remote)
{
    slurm_debug("%s: %s val=%d optarg=%s remote=%d",
            plugin_name, __FUNCTION__, val, optarg, remote);
    strncpy_s(primitive_type, optarg, sizeof(primitive_type));
    return ESPANK_SUCCESS;
}

/* Callback: parse conf JSON now and store all env key/values */
static int qrmi_conf_cb(int val, const char *optarg, int remote) {
    (void)val; (void)remote;
    /* Save path */
    strncpy_s(qrmi_conf_path, optarg, sizeof(qrmi_conf_path));
    /* Reload conf_list */
    //free_conf_list();

    /* Read file */
    FILE *fp = fopen(optarg, "rb");
    if (!fp) return ESPANK_SUCCESS;
    struct stat st;
    if (fstat(fileno(fp), &st) != 0 || st.st_size <= 0 || st.st_size > MAX_JSON_FILE) {
        fclose(fp); return ESPANK_SUCCESS;
    }
    char *json = malloc(st.st_size + 1);
    size_t n = fread(json, 1, st.st_size, fp);
    json[n] = '\0';
    fclose(fp);

    /* Locate env block */
    char *env_pos = strstr(json, "\"env\"");
    if (env_pos) {
        env_pos = strchr(env_pos, '{');
        if (env_pos) {
            const char *p = env_pos + 1;
            while (*p && *p != '}') {
                char *k = NULL, *v = NULL;
                const char *next = parse_kv(p, &k, &v);
                if (!next) break;
                /* Store in list */
                kv_node_t *node = malloc(sizeof(*node));
                node->key = k;
                node->val = v;
                node->next = conf_list;
                conf_list = node;
                p = next;
            }
        }
    }
    free(json);
    return ESPANK_SUCCESS;
}

/* Helper: get value from global list */
static const char *conf_get(const char *key) {
    for (kv_node_t *n = conf_list; n; n = n->next) {
        if (strcmp(n->key, key) == 0) return n->val;
    }
    return NULL;
}

/* Inject env from list */
static void inject_env_from_conf(spank_t ctxt) {
    for (kv_node_t *n = conf_list; n; n = n->next) {
        spank_setenv(ctxt, n->key, n->val, 1);
        setenv(n->key, n->val, 1);
    }
}

struct spank_option spank_options[] = {
    {"q-backend",   "name", "Name of Qiskit backend.",                     1,0,(spank_opt_cb_f)backend_name_cb},
    {"q-primitive", "type", "Qiskit primitive type (sampler|estimator).", 1,0,(spank_opt_cb_f)primitive_type_cb},
    {"qrmi-conf",   "path", "Path to QRMI JSON config.",                  1,0,(spank_opt_cb_f)qrmi_conf_cb},
    SPANK_OPTIONS_TABLE_END
};

int slurm_spank_init(spank_t ctxt,int argc,char**argv) {
    job_info_msg_t *job_info_msg = NULL;
    uint32_t job_id = 0;
    memset(backend_name,0,sizeof backend_name);
    memset(primitive_type,0,sizeof primitive_type);
    memset(qrun_job_id,0,sizeof qrun_job_id);
    memset(qrmi_conf_path,0,sizeof qrmi_conf_path);
    memset(token, '\0', sizeof(token));
    int rc = ESPANK_SUCCESS;
    slurm_debug("FUGAFUGA");
    for (struct spank_option *o = spank_options; o->name && rc == ESPANK_SUCCESS; o++)
        rc = spank_option_register(ctxt, o);
    if (spank_remote(ctxt)) {
            slurm_info("REMOTE");
    } else {
            slurm_info("LOCAL HOGEHOGE");
    }
    return rc;
}


int slurm_spank_init_post_opt(spank_t ctxt,int argc,char**argv) {
    job_info_msg_t *job_info_msg = NULL;
    uint32_t job_id = 0;
    slurm_debug("INITNITNI");
    if (spank_remote(ctxt)) {
        for (kv_node_t *n = conf_list; n; n = n->next) {
            slurm_debug("setenv %s:%s", n->key, n->val);
            spank_setenv(ctxt, n->key, n->val, 1);
            setenv(n->key, n->val, 1);
        }
        if (*primitive_type)
            spank_setenv(ctxt, "IBMQRUN_PRIMITIVE", primitive_type, 1);
        if (*backend_name) {
            spank_setenv(ctxt, "QRMI_RESOURCE_ID", backend_name, 1);
            spank_setenv(ctxt, "IBMQRUN_BACKEND", backend_name, 1);
            setenv("QRMI_RESOURCE_ID", backend_name, 1);
        }
        if (spank_get_item(ctxt, S_JOB_ID, &job_id) == ESPANK_SUCCESS) { 
            if (slurm_load_job(&job_info_msg, job_id, SHOW_DETAIL) == SLURM_SUCCESS) {
                /* slurm's time limit is represented in minutes */
                uint32_t time_limit_mins = job_info_msg->job_array[0].time_limit;
                /*
                * minutes to seconds, uint32_t to char*
                */
                char limit_as_str[11]; /* max uint32_t value is (2147483647) = 10 chars */
                memset(limit_as_str, '\0', sizeof(limit_as_str));
                snprintf(limit_as_str, sizeof(limit_as_str), "%u", time_limit_mins * 60);
                spank_setenv(ctxt, "QRMI_IBM_DA_TIMEOUT_SECONDS", limit_as_str, 1);
                setenv("QRMI_IBM_DA_TIMEOUT_SECONDS", limit_as_str, 1);
            }
        }
        IBMDirectAccess *qrmi = qrmi_ibmda_new();
        //const char *acquisition_token = qrmi_ibmda_acquire(qrmi, backend_name);
        const char *get_token = (qrmi_ibmda_acquire(qrmi, backend_name));
        strncpy_s(token, get_token, sizeof(token));
        /* Store token in conf_list for exit release */
        {
            kv_node_t *t_node = malloc(sizeof(*t_node));
            t_node->key = strdup("QRMI_DA_TOKEN");
            t_node->val = strdup(token);
            t_node->next = conf_list;
            conf_list = t_node;
        }
        spank_setenv(ctxt, "QRMI_DA_TOKEN", token, 1);
        setenv("QRMI_DA_TOKEN", token, 1);
        slurm_debug("acquisition_token = %s\n", token);
        slurm_debug("return qrmi");
        //qrmi_ibmda_free(qrmi);
    }
    return ESPANK_SUCCESS;
}

int slurm_spank_task_exit(spank_t ctxt,int a,char**b) {
    if (spank_remote(ctxt)) {
        char token_buf[1024] = {0};
        const char *token_env = getenv("QRMI_DA_TOKEN");
        slurm_debug("EXIT QRMI_DA_TOKEN = %s", token_env);
        slurm_debug("EXIT QRMI_DA_TOKEN = %s", token);
        slurm_debug("HOGEHO %s", conf_get("QRMI_DA_TOKEN"));
        if (spank_getenv(ctxt, "QRMI_DA_TOKEN", token_buf, sizeof(token_buf)) == ESPANK_SUCCESS && token_buf[0] != '\0') {
            slurm_debug("EXIT QRMI_DA_TOKEN = %s", token_buf);
            IBMDirectAccess *qrmi = qrmi_ibmda_new();
            qrmi_ibmda_release(qrmi, token_buf);
            slurm_debug("released QRMI_DA_TOKEN %s", token_buf);
            qrmi_ibmda_free(qrmi);
        } else {
            slurm_error("QRMI_DA_TOKEN not found in environment for release");
        }
        /* cleanup conf_list */
        free_conf_list();
        /* unset plugin environment variables */
        spank_unsetenv(ctxt, "IBMQRUN_BACKEND");
        spank_unsetenv(ctxt, "IBMQRUN_PRIMITIVE");
    }
    return ESPANK_SUCCESS;
}
