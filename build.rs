use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIDECAR_MAIN: &str = "dev.jdbg.sidecar.SidecarMain";
const SIDECAR_JAR: &str = "jdbg-jdi-sidecar.jar";

fn main() {
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    println!("cargo:rerun-if-env-changed=JDBG_SKIP_JDI_SIDECAR_BUILD");
    println!("cargo:rerun-if-changed=sidecar/jdi/src/main/java");

    if env::var_os("JDBG_SKIP_JDI_SIDECAR_BUILD").is_some() {
        println!(
            "cargo:warning=skipping JDI sidecar build because JDBG_SKIP_JDI_SIDECAR_BUILD is set"
        );
        return;
    }

    let Some(javac) = find_java_tool("javac") else {
        println!("cargo:warning=javac not found; JDI sidecar jar will not be built");
        return;
    };
    let Some(jar) = find_java_tool("jar") else {
        println!("cargo:warning=jar not found; JDI sidecar jar will not be built");
        return;
    };

    let source_root = PathBuf::from("sidecar")
        .join("jdi")
        .join("src")
        .join("main")
        .join("java");
    let sources = java_sources(&source_root);
    if sources.is_empty() {
        println!("cargo:warning=no JDI sidecar Java sources found");
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let classes_dir = out_dir.join("jdi-sidecar-classes");
    let profile_dir = profile_dir_from_out_dir(&out_dir);
    let jar_path = profile_dir.join(SIDECAR_JAR);

    let _ = std::fs::remove_dir_all(&classes_dir);
    std::fs::create_dir_all(&classes_dir).expect("create sidecar classes dir");

    let mut javac_cmd = Command::new(&javac);
    javac_cmd.arg("-encoding").arg("UTF-8");
    if let Some(tools_jar) = tools_jar() {
        javac_cmd.arg("-cp").arg(tools_jar);
    }
    javac_cmd.arg("-d").arg(&classes_dir).args(&sources);
    run_or_panic(javac_cmd, "javac JDI sidecar");

    let mut jar_cmd = Command::new(&jar);
    jar_cmd
        .arg("cfe")
        .arg(&jar_path)
        .arg(SIDECAR_MAIN)
        .arg("-C")
        .arg(&classes_dir)
        .arg(".");
    run_or_panic(jar_cmd, "jar JDI sidecar");
}

fn run_or_panic(mut command: Command, label: &str) {
    let output = command.output().unwrap_or_else(|e| {
        panic!("failed to run {label}: {e}");
    });
    if !output.status.success() {
        panic!(
            "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn find_java_tool(name: &str) -> Option<PathBuf> {
    let exe = java_exe_name(name);
    if let Some(home) = env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(&exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_in_path(&exe)
}

fn java_exe_name(name: &str) -> OsString {
    if cfg!(windows) {
        OsString::from(format!("{name}.exe"))
    } else {
        OsString::from(name)
    }
}

fn find_in_path(exe: &OsString) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

fn tools_jar() -> Option<PathBuf> {
    env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("lib").join("tools.jar"))
        .filter(|path| path.is_file())
}

fn java_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_java_sources(root, &mut out);
    out
}

fn collect_java_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_java_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "java") {
            out.push(path);
        }
    }
}

fn profile_dir_from_out_dir(out_dir: &Path) -> PathBuf {
    out_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("OUT_DIR should be target/<profile>/build/<pkg>/out")
}
