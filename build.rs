use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIDECAR_JAR: &str = "jdbg-jdi-sidecar.jar";
const MIN_GRADLE_JAVA_MAJOR: u32 = 17;

fn main() {
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    println!("cargo:rerun-if-env-changed=JDBG_GRADLE_JAVA_HOME");
    println!("cargo:rerun-if-env-changed=JDBG_SKIP_JDI_SIDECAR_BUILD");
    println!("cargo:rerun-if-changed=sidecar/jdi/build.gradle");
    println!("cargo:rerun-if-changed=sidecar/jdi/settings.gradle");
    println!("cargo:rerun-if-changed=sidecar/jdi/gradle/wrapper/gradle-wrapper.properties");
    let java_src_dir = Path::new("sidecar")
        .join("jdi")
        .join("src")
        .join("main")
        .join("java");
    emit_rerun_for_dir(&java_src_dir);

    if env::var_os("JDBG_SKIP_JDI_SIDECAR_BUILD").is_some() {
        println!(
            "cargo:warning=skipping JDI sidecar build because JDBG_SKIP_JDI_SIDECAR_BUILD is set"
        );
        return;
    }

    let Some(gradle_java_home) = find_gradle_java_home() else {
        println!(
            "cargo:warning=JDK {MIN_GRADLE_JAVA_MAJOR}+ not found; JDI sidecar jar will not be built"
        );
        return;
    };

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let profile_dir = profile_dir_from_out_dir(&out_dir);
    let jar_path = profile_dir.join(SIDECAR_JAR);
    let sidecar_dir = PathBuf::from("sidecar").join("jdi");

    let mut gradle_cmd = gradle_wrapper_command(&sidecar_dir);
    gradle_cmd
        .current_dir(&sidecar_dir)
        .arg("--no-daemon")
        .arg("jar")
        .env("JAVA_HOME", &gradle_java_home);
    if let Some(path) = gradle_path_with_java_home(&gradle_java_home) {
        gradle_cmd.env("PATH", path);
    }
    run_or_panic(gradle_cmd, "Gradle JDI sidecar build");

    let built_jar = sidecar_dir.join("build").join("libs").join(SIDECAR_JAR);
    std::fs::create_dir_all(&profile_dir).expect("create cargo profile dir");
    std::fs::copy(&built_jar, &jar_path).unwrap_or_else(|e| {
        panic!(
            "failed to copy JDI sidecar jar from {} to {}: {e}",
            built_jar.display(),
            jar_path.display()
        )
    });
}

fn emit_rerun_for_dir(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            emit_rerun_for_dir(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
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

fn gradle_wrapper_command(sidecar_dir: &Path) -> Command {
    if cfg!(windows) {
        Command::new(sidecar_dir.join("gradlew.bat"))
    } else {
        let mut command = Command::new("sh");
        command.arg("./gradlew");
        command
    }
}

fn java_exe_name(name: &str) -> OsString {
    if cfg!(windows) {
        OsString::from(format!("{name}.exe"))
    } else {
        OsString::from(name)
    }
}

fn find_gradle_java_home() -> Option<PathBuf> {
    if let Some(home) = env::var_os("JDBG_GRADLE_JAVA_HOME").map(PathBuf::from) {
        if java_home_major(&home).is_some_and(|major| major >= MIN_GRADLE_JAVA_MAJOR) {
            return Some(home);
        }
        panic!(
            "JDBG_GRADLE_JAVA_HOME must point to JDK {MIN_GRADLE_JAVA_MAJOR}+; got {}",
            home.display()
        );
    }

    if let Some(home) = env::var_os("JAVA_HOME").map(PathBuf::from) {
        if java_home_major(&home).is_some_and(|major| major >= MIN_GRADLE_JAVA_MAJOR) {
            return Some(home);
        }
    }

    common_jdk_homes()
        .into_iter()
        .find(|home| java_home_major(home).is_some_and(|major| major >= MIN_GRADLE_JAVA_MAJOR))
}

fn java_home_major(home: &Path) -> Option<u32> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    for line in release.lines() {
        let Some(version) = line.strip_prefix("JAVA_VERSION=\"") else {
            continue;
        };
        let version = version.trim_end_matches('"');
        let mut parts = version.split(['.', '_']);
        let first = parts.next()?.parse::<u32>().ok()?;
        if first == 1 {
            return parts.next()?.parse().ok();
        }
        return Some(first);
    }
    None
}

fn common_jdk_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    for parent in common_jdk_parents() {
        let Ok(entries) = std::fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("bin").join(java_exe_name("java")).is_file() {
                homes.push(path.clone());
            }
            let bundled = path.join("Contents").join("Home");
            if bundled.join("bin").join(java_exe_name("java")).is_file() {
                homes.push(bundled);
            }
        }
    }
    homes
}

fn common_jdk_parents() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        dirs.push(PathBuf::from(&home).join(".jdks"));
    }
    #[cfg(windows)]
    {
        dirs.push(PathBuf::from(r"C:\Program Files\Java"));
        dirs.push(PathBuf::from(r"C:\Program Files\Eclipse Adoptium"));
        dirs.push(PathBuf::from(r"C:\Program Files\Microsoft"));
    }
    #[cfg(not(windows))]
    {
        dirs.push(PathBuf::from("/usr/lib/jvm"));
        dirs.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
    }
    dirs
}

fn gradle_path_with_java_home(java_home: &Path) -> Option<OsString> {
    let mut paths = vec![java_home.join("bin")];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).ok()
}

fn profile_dir_from_out_dir(out_dir: &Path) -> PathBuf {
    out_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("OUT_DIR should be target/<profile>/build/<pkg>/out")
}
