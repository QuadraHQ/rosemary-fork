//! The guest-facing half of `rosemary::hostbridge`: a Mach-O program loads host
//! libraries and calls into them, on its main thread and on a thread of its own.

mod common;

const BRIDGE: &str = r#"
extern int printf(const char *, ...);
extern char *getenv(const char *);
extern int strcmp(const char *, const char *);
extern int pthread_create(void **, const void *, void *(*)(void *), void *);
extern int pthread_join(void *, void **);

extern int quadra_host_native(void);
extern void *quadra_host_dlopen(const char *, int);
extern void *quadra_host_dlsym(void *, const char *);
extern const char *quadra_host_dlerror(void);

#define RTLD_NOW 2
#define RTLD_GLOBAL 8

static void *libc;
static char *(*host_getenv)(const char *);
static int (*host_setenv)(const char *, const char *, int);
static void (*host_qsort)(void *, unsigned long, unsigned long, int (*)(const void *, const void *));
static void *(*host_malloc)(unsigned long);
static void (*host_free)(void *);
static char *(*host_strerror)(int);

static int compare(const void *a, const void *b) {
    int x = *(const int *)a, y = *(const int *)b;
    return (x > y) - (x < y);
}

static int worker_ok;
static void *worker(void *arg) {
    (void)arg;
    int values[5] = {5, 3, 9, 1, 7};
    char *block = host_malloc(64);
    host_qsort(values, 5, sizeof values[0], compare); /* the host calls back into the guest */
    const char *text = host_strerror(2);
    host_free(block);
    worker_ok = block && values[0] == 1 && values[4] == 9 && text && text[0] == 'N' /* No such file */
                && host_getenv("QUADRA_BRIDGE_PROBE") != 0;
    return 0;
}

int main(void) {
    printf("native: %d\n", quadra_host_native());
    libc = quadra_host_dlopen("libc.so.6", RTLD_NOW);
    if (!libc) { printf("no libc: %s\n", quadra_host_dlerror()); return 1; }
    host_getenv = quadra_host_dlsym(libc, "getenv");
    host_setenv = quadra_host_dlsym(libc, "setenv");
    host_qsort = quadra_host_dlsym(libc, "qsort");
    host_malloc = quadra_host_dlsym(libc, "malloc");
    host_free = quadra_host_dlsym(libc, "free");
    host_strerror = quadra_host_dlsym(libc, "strerror");
    host_setenv("QUADRA_BRIDGE_PROBE", "1", 1);
    printf("host sees probe: %s\n", host_getenv("QUADRA_BRIDGE_PROBE") ? "yes" : "no");
    printf("guest sees probe: %s\n", getenv("QUADRA_BRIDGE_PROBE") ? "yes" : "no");
    const char *host_home = host_getenv("HOME"), *guest_home = getenv("HOME");
    printf("homes differ: %s\n", host_home && guest_home && strcmp(host_home, guest_home) ? "yes" : "no");
    void *thread;
    pthread_create(&thread, 0, worker, 0);
    pthread_join(thread, 0);
    printf("secondary thread: %s\n", worker_ok ? "ok" : "failed");
    void *egl = quadra_host_dlopen("libEGL.so.1", RTLD_NOW);
    void *(*get_proc)(const char *) = egl ? quadra_host_dlsym(egl, "eglGetProcAddress") : 0;
    printf("egl: %s\n", get_proc && get_proc("eglGetCurrentContext") ? "ok" : "missing");
    void *missing = quadra_host_dlopen("libdoesnotexist.so.9", RTLD_NOW);
    printf("missing library: %s, error %s\n", missing ? "opened" : "null", quadra_host_dlerror() ? "set" : "unset");
    printf("missing symbol: %s\n", quadra_host_dlsym(libc, "no_such_symbol_here") ? "found" : "null");
    return 0;
}
"#;

/// Needs the native backend, whose guest instructions are the host's.
fn native_host() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Linked the way the bridge's users are: no library defines `quadra_host_*`
/// at link time, so they are bound by flat lookup at load.
fn probe() -> Option<String> {
    common::build_x86_64(
        "hostbridge_probe",
        BRIDGE,
        &["-Wl,-undefined,dynamic_lookup"],
    )
}

#[test]
fn host_libraries_are_called_directly_from_native_threads() {
    if !native_host() {
        return;
    }
    let Some(program) = probe() else {
        return;
    };
    let (code, out, err) = common::run_guest_with(&["--native"], &program, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "native: 1\n\
         host sees probe: yes\n\
         guest sees probe: no\n\
         homes differ: yes\n\
         secondary thread: ok\n\
         egl: ok\n\
         missing library: null, error set\n\
         missing symbol: null\n"
    );
}

#[test]
fn emulated_backend_refuses_host_libraries() {
    let Some(program) = probe() else {
        return;
    };
    let (_, out, _) = common::run_guest_with(&["--emulated"], &program, &[]);
    assert_eq!(
        out,
        "native: 0\nno libc: host libraries can only be called from the native backend\n"
    );
}

#[test]
fn no_passthrough_refuses_host_libraries() {
    if !native_host() {
        return;
    }
    let Some(program) = probe() else {
        return;
    };
    let out = common::driver_command()
        .args(["run", "--native", &program])
        .env("ROSEMARY_NO_PASSTHROUGH", "1")
        .output()
        .expect("running the driver");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "native: 0\nno libc: refused by ROSEMARY_NO_PASSTHROUGH\n"
    );
}
