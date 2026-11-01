mod common;

const INSERTED: &str = r#"
extern int puts(const char *);
__attribute__((constructor)) static void init(void) { puts("inserted"); }
"#;

const MAIN: &str = r#"
extern int puts(const char *);
int main(void) { puts("main"); return 0; }
"#;

#[test]
fn host_variable_inserts_a_library() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = common::build_x86_64("insert_main", MAIN, &[]) else {
        return;
    };
    let (_, out, _) =
        common::run_guest_env(&[("ROSEMARY_DYLD_INSERT_LIBRARIES", &lib)], &main, &[]);
    assert_eq!(out, "inserted\nmain\n");
}

#[test]
fn guest_variable_is_the_one_that_counts() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = common::build_x86_64("insert_main", MAIN, &[]) else {
        return;
    };
    // The host's spelling is not the guest's, so it inserts nothing.
    let (_, out, _) = common::run_guest_env(&[("DYLD_INSERT_LIBRARIES", &lib)], &main, &[]);
    assert_eq!(out, "main\n");
    let line = format!("DYLD_INSERT_LIBRARIES={lib}");
    let (_, out, _) = common::run_guest_env(&[("ROSEMARY_ENVIRON", &line)], &main, &[]);
    assert_eq!(out, "inserted\nmain\n");
}

/// A program with a `__RESTRICT,__restrict` section, which dyld treats as
/// closed to `DYLD_*` variables.
fn restricted_main() -> Option<String> {
    let marker = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("restrict_marker");
    std::fs::write(&marker, "restricted").ok()?;
    let section = format!("-Wl,-sectcreate,__RESTRICT,__restrict,{}", marker.display());
    common::build_x86_64("insert_restricted_main", MAIN, &[&section])
}

#[test]
fn restricted_image_refuses_insertion() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = restricted_main() else {
        return;
    };
    let (_, out, err) =
        common::run_guest_env(&[("ROSEMARY_DYLD_INSERT_LIBRARIES", &lib)], &main, &[]);
    assert_eq!(out, "main\n");
    assert!(err.contains("DYLD_INSERT_LIBRARIES ignored"), "{err}");
}

#[test]
fn force_variable_inserts_into_a_restricted_image() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = restricted_main() else {
        return;
    };
    let env = [
        ("ROSEMARY_DYLD_INSERT_LIBRARIES", lib.as_str()),
        ("ROSEMARY_DYLD_INSERT_FORCE", "1"),
    ];
    let (_, out, _) = common::run_guest_env(&env, &main, &[]);
    assert_eq!(out, "inserted\nmain\n");
}

#[test]
fn guest_cannot_force_insertion_into_a_restricted_image() {
    let Some(lib) = common::build_x86_64("insert_lib", INSERTED, &["-dynamiclib"]) else {
        return;
    };
    let Some(main) = restricted_main() else {
        return;
    };
    let environ = format!("DYLD_INSERT_LIBRARIES={lib}\nROSEMARY_DYLD_INSERT_FORCE=1");
    let (_, out, _) = common::run_guest_env(&[("ROSEMARY_ENVIRON", &environ)], &main, &[]);
    assert_eq!(out, "main\n");
}
