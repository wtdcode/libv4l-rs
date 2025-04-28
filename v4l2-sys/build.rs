extern crate bindgen;

use std::env;
use std::path::{Path, PathBuf};

fn print_cc(cc: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new(cc.to_str().expect("utf-8?"))
        .args(args)
        .output()
        .expect("fail to run cc");
    if !out.status.success() {
        panic!(
            "sysroot not found with {} with {:?}",
            String::from_utf8_lossy(&out.stderr),
            args
        );
    }
    String::from_utf8(out.stdout).expect("non utf-8?!")
}

fn main() {
    let extra_include_paths = if cfg!(target_os = "freebsd") {
        assert!(
            Path::new("/usr/local/include/linux/videodev2.h").exists(),
            "Video4Linux `videodev2.h` UAPI header is required to generate bindings \
            against `libv4l2` and the header file is missing.\n\
            Consider installing `multimedia/v4l_compat` FreeBSD package."
        );
        vec!["-I/usr/local/include".to_string()]
    } else {
        vec![]
    };

    let cc = cc::Build::new().get_compiler();

    let cc_sysroot = print_cc(cc.path(), &["--print-sysroot"]).trim().to_string();
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(
            extra_include_paths.into_iter().chain(
                vec![
                    format!("--sysroot={}", &cc_sysroot),
                    format!("-I{}/usr/include/linux", cc_sysroot),
                ]
                .into_iter(),
            ),
        )
        .generate()
        .expect("Failed to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("v4l2_bindings.rs"))
        .expect("Failed to write bindings");
}
