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
#include <stdint.h>

#include "slurm/slurm.h"
#include "slurm/spank.h"
#include "qrmi.h"

SPANK_PLUGIN(spank_ibm_qrmi, 1)

#define MAXLEN_BACKEND_NAME 256
#define MAXLEN_PROGRAM_ID   256
#define MAXLEN_JOB_ID      1024
#define MAX_JSON_FILE     65536
#define MAXLEN_TOKEN       1024

/* Globals for options */
static char backend_name [MAXLEN_BACKEND_NAME + 1];
static char primitive_type[MAXLEN_PROGRAM_ID  + 1];
static char qrun_job_id   [MAXLEN_JOB_ID      + 1];
static char qrmi_conf_path[PATH_MAX];
static char token[MAXLEN_TOKEN];

/* Global pointer for qrmi */
IBMDirectAccess *qrmi = NULL;

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


/*
 * @function backend_name_cb
 *
 * Callback function to check --q-backend option value.
 *
 */
static int backend_name_cb(int val, const char *optarg, int remote)
{
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

/*
 * @function qrmi_conf_cb
 *
 * Function to check qrmi-conf option value.
 *
 */
static int qrmi_conf_cb(const char *optarg) {
    slurm_info("FILE %s", optarg);
    /* Read file */
    FILE *fp = fopen(optarg, "r");
    if (!fp) {
        slurm_info("No such file (%s).", optarg);
        return ESPANK_SUCCESS;
    }

    char line[MAX_JSON_FILE];
    while (fgets(line, sizeof(line), fp)) {
        char *saveptr = NULL, *token = NULL;
        /* tokenize by whitespace */
        token = strtok_r(line, " \t\r\n", &saveptr);
        while (token != NULL) {
            /* find '=' */
            char *eq = strchr(token, '=');
            if (eq) {
                *eq = '\0';
                char *k = strdup(token);
                char *v = strdup(eq + 1);
                if (k && v) {
                    kv_node_t *node = malloc(sizeof(*node));
                    if (node) {
                        node->key   = k;
                        node->val   = v;
                        node->next  = conf_list;
                        conf_list   = node;
                    } else {
                        free(k); free(v);
                    }
                }
            }
            token = strtok_r(NULL, " \t\r\n", &saveptr);
        }
    }

    fclose(fp);
    return ESPANK_SUCCESS;
}


struct spank_option spank_options[] = {
    {"q-backend",   "name", "Name of Qiskit backend.",                    1,0,(spank_opt_cb_f)backend_name_cb},
    {"q-primitive", "type", "Qiskit primitive type (sampler|estimator).", 1,0,(spank_opt_cb_f)primitive_type_cb},
    SPANK_OPTIONS_TABLE_END
};

int slurm_spank_init(spank_t ctxt,int argc,char**argv) {
    int i;

    memset(backend_name,0,sizeof backend_name);
    memset(primitive_type,0,sizeof primitive_type);
    memset(qrun_job_id,0,sizeof qrun_job_id);
    memset(token, '\0', sizeof(token));
    int rc = ESPANK_SUCCESS;

    for (i = 0; i < argc; i++) {
        if (strncmp ("qrmi-conf=", argv[i], 10) == 0) {
            const char *optarg = argv[i] + 10;
            qrmi_conf_cb(optarg);
        }
    }

    for (struct spank_option *o = spank_options; o->name && rc == ESPANK_SUCCESS; o++)
        rc = spank_option_register(ctxt, o);

    return rc;
}


int slurm_spank_init_post_opt(spank_t ctxt,int argc,char**argv) {
    job_info_msg_t *job_info_msg = NULL;
    uint32_t job_id = 0;
    uint32_t step_id = 0;

    if (spank_remote(ctxt)) {
        for (kv_node_t *n = conf_list; n; n = n->next) {
            slurm_debug("setenv %s : %s", n->key, n->val);
            spank_setenv(ctxt, n->key, n->val, 1);
        }
        if (*backend_name) {
            spank_setenv(ctxt, "QRMI_RESOURCE_ID", backend_name, 1);
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
            }
        }
        spank_get_item(ctxt, S_JOB_STEPID, &step_id);
        slurm_debug("STEP ID = 0x%08X", step_id);
        if (step_id  == SLURM_BATCH_SCRIPT){
            qrmi = qrmi_ibmda_new();
            const char *get_token = (qrmi_ibmda_acquire(qrmi, backend_name));
            strncpy_s(token, get_token, sizeof(token));
            slurm_debug("Get Token %s", token);
        }
    }
    return ESPANK_SUCCESS;
}

int slurm_spank_task_exit(spank_t ctxt,int a,char**b) {
    if (spank_remote(ctxt)) {
        if (qrmi && token != 0){
            slurm_debug("Release Token %s", token);
            qrmi_ibmda_release(qrmi, token);
            qrmi_ibmda_free(qrmi);
        } 
        /* cleanup conf_list */
        free_conf_list();
    }
    return ESPANK_SUCCESS;
}
