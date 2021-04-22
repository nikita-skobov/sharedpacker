use exechelper;
use gumdrop::Options;
use std::{ops::Deref, path::{Path, PathBuf}, collections::HashMap};

#[derive(Debug, Options)]
pub struct Cli {
    /// prints thaaa helpp
    pub help: bool,

    /// print detailed logging info to stderr
    pub verbose: bool,

    /// name of folder to be created that will contain
    /// the archive of all of the shared libs
    #[options(short = "o")]
    pub output: Option<PathBuf>,

    /// if the output archive already exists by default we exit with an error
    /// and a message. if you pass the --force flag, we will override it
    #[options(short = "f")]
    pub force: bool,

    #[options(free)]
    pub exepath: Vec<PathBuf>
}

#[derive(Debug)]
pub struct SharedLib {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct DependencyNode {
    pub name: String,
    pub path: PathBuf,
    pub dependencies: Vec<String>,
}

pub fn get_lib_path_list(
    path: &Path,
) -> Result<Vec<SharedLib>, String> {
    let strthing: &str = path.to_str().map_or_else(|| Err("Failed to get path as string"), |o| Ok(o))?;
    let exec_args = [
        "ldd", strthing,
    ];
    let output = exechelper::execute(&exec_args)
        .map_err(|e| e.to_string())?;
    if output.status != 0 {
        return Err(output.stderr);
    }

    // eprintln!("GOT OUTPUT: \n{}", output.stdout);
    let mut outvec = vec![];

    // rules for parsing ldd output:
    // - must start with at least one empty whitespace char
    //   because its possible ldd might display some header info that
    //   we dont want to parse
    // - must contain an arrow '=>' otherwise it is something thats statically linked?
    for line in output.stdout.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            continue;
        }
        let no_whitespace = line.trim_start().trim_end();
        if !no_whitespace.contains(" => ") {
            continue;
        }

        let mut split = no_whitespace.split(" => ");
        let libname = split.next().map_or_else(|| Err("Failed to parse ldd output"), |l| Ok(l))?;
        let pathpart = split.next().map_or_else(|| Err("Failed to parse ldd output"), |l| Ok(l))?;
        if pathpart.contains("not found") {
            return Err(format!("Dependency on {} is not found", libname));
        }

        // TODO: should we ignore the loader or not?
        if libname.starts_with('/') {
            continue;
        }

        // let libname = if libname.starts_with('/') {
        //     // if this is the loader it will usually start with /
        //     // so we want to remove its base bath and just have the file name
        //     libname.rsplit('/').next().unwrap_or(libname)
        // } else { libname };

        let pathpart = match pathpart.find(' ') {
            None => pathpart,
            Some(index) => {
                &pathpart[0..index]
            }
        };
        outvec.push(SharedLib {
            name: libname.into(),
            path: pathpart.into(),
        });
    }

    Ok(outvec)
}

/// use patchelf to find a list of needed libs from an executable
pub fn get_needed_libs(
    path: &Path
) -> Result<Vec<String>, String> {
    let strthing: &str = path.to_str().map_or_else(|| Err("Failed to get path as string"), |o| Ok(o))?;
    let exec_args = [
        "patchelf", "--print-needed", strthing,
    ];
    let output = exechelper::execute(&exec_args)
        .map_err(|e| e.to_string())?;
    if output.status != 0 {
        return Err(output.stderr);
    }

    let mut outvec = vec![];
    for line in output.stdout.lines() {
        let trimmed: String = line.trim_start().trim_end().into();
        // TODO: should ignore loader or not?
        if trimmed.starts_with("ld-linux") {
            continue;
        }
        outvec.push(trimmed);
    }

    Ok(outvec)
}

pub fn traverse_dependencies(
    known_lib_location_map: &mut HashMap<String, PathBuf>,
    use_libs: &mut Vec<String>,
    dependency_nodes: &mut Vec<DependencyNode>,
    needed_path: &Path,
    needed_name: &str,
    verbose: bool,
    log_prefix: &str,
) -> Result<(), String> {
    // eprintln!("Looking for needed: {:?}", needed_path);
    // first we iterate over its dependencies, and add the known paths
    // to our map:
    let shared_libs = match get_lib_path_list(needed_path) {
        Ok(l) => l,
        Err(e) => return Err(e),
    };
    for lib in shared_libs {
        // eprintln!("PATH: {:?}", lib);
        if !known_lib_location_map.contains_key(&lib.name) {
            known_lib_location_map.insert(lib.name, lib.path);
        }
    }

    let mut dependency_node = DependencyNode {
        name: needed_name.into(),
        path: needed_path.into(),
        dependencies: vec![]
    };
    // next we get all of the actually needed dependencies of this file
    // and for each dependency, we recurse and do this process again, each
    // time appending the use_libs list of libs that we will ultimately use
    let needed_shared_libs = get_needed_libs(needed_path)?;
    for lib in needed_shared_libs {
        dependency_node.dependencies.push(lib.clone());

        // find this libs path from our map
        let lib_path = match known_lib_location_map.get(&lib) {
            Some(p) => p.clone(),
            None => {
                return Err(format!("Found needed library that we don't know a location of: {}", lib));
            }
        };

        // dont recurse for a lib name that weve already found
        if !use_libs.contains(&lib) {
            let next_log_prefix = format!("{}  ", log_prefix);
            if verbose {
                eprintln!("{}{} => {:?}", next_log_prefix, lib, lib_path);
            }

            // prevent duplicates (yes its inefficient, but
            use_libs.push(lib.clone());

            traverse_dependencies(
                known_lib_location_map, use_libs, dependency_nodes,
                &lib_path, &lib, verbose, &next_log_prefix)?;
        }
    }

    dependency_nodes.push(dependency_node);
    Ok(())
}

pub fn cleanup_if_err(archive_path: &PathBuf) {
    let _ = std::fs::remove_dir_all(archive_path);
}

pub fn patch_shared_lib(
    libname: &str,
    object_path: &PathBuf
) -> Result<(), String> {
    let new_name = format!("./{}", libname);
    let obj_path_str = object_path.to_string_lossy().to_string();
    let exec_args = [
        "patchelf", "--replace-needed", libname, &new_name[..], &obj_path_str
    ];
    let output = exechelper::execute(&exec_args).map_err(|e| e.to_string())?;
    if output.status != 0 {
        return Err(output.stderr);
    }

    Ok(())
}

pub fn patch_loader(
    loader: &str,
    object_path: &PathBuf,
) -> Result<(), String> {
    let new_name = format!("./{}", loader);
    let obj_path_str = object_path.to_string_lossy().to_string();
    let exec_args = [
        "patchelf", "--set-interpreter", &new_name, "--set-rpath", ".", &obj_path_str
    ];
    let output = exechelper::execute(&exec_args).map_err(|e| e.to_string())?;
    if output.status != 0 {
        // patchelf can give error:
        // cannot find section '.interp'. The input file is most likely statically linked
        // when the linker is statically linked. ignore this error
        if output.stderr.contains("statically linked") {
            return Ok(())
        }
        return Err(format!("Failed to patch loader for {:?}\n{}", object_path, output.stderr));
    }

    Ok(())
}

pub fn copy_dependencies_to_output_folder(
    archive_path: &PathBuf,
    dependencies: &Vec<DependencyNode>,
) -> Result<(), String> {
    std::fs::create_dir_all(&archive_path).map_err(|e| e.to_string())?;

    // find the linker name
    // TODO:
    let linker_name = "ld-linux-x86-64.so.2";

    for dep in dependencies {
        let dep_path = &dep.path;
        let filename = dep_path.file_name()
            .map_or_else(|| Err(format!("Failed to find file name for {:?}", dep_path)), |o| Ok(o))?;
        let mut output_path = archive_path.clone();
        output_path.push(filename);

        std::fs::copy(dep_path, &output_path)
            .map_err(|e| format!("Failed to copy {:?} to {:?}\n{}", dep_path, output_path, e))?;

        // now patch this file's dynamic section with all of its dependencies:
        // for child_dep in &dep.dependencies {
        //     patch_shared_lib(&child_dep, &output_path)?;
        // }
        patch_loader(linker_name, &output_path)?;
    }

    Ok(())
}

fn main() {
    let cli = <Cli as Options>::parse_args_default_or_exit();
    let execpath = match cli.exepath.get(0) {
        Some(o) => o,
        None => {            
            let usage = cli.self_usage();
            eprintln!("Must provide at least one path to an executable\n{}", usage);
            std::process::exit(1);
        }
    };
    if cli.verbose {
        eprintln!("{:#?}\n", cli);
    }
    let mut lib_location_map = HashMap::new();
    let mut used_libs = vec![];
    let mut dependencies = vec![];

    if cli.verbose {
        eprintln!("{:?}", execpath);
    }

    // we also copy the original exec path given to us
    let execname = execpath.file_name().unwrap_or_else(|| {
        eprintln!("Failed to get exec path file name from {:?}", execpath);
        std::process::exit(1);
    }).to_string_lossy().to_string();

    if let Err(e) = traverse_dependencies(
        &mut lib_location_map, &mut used_libs, &mut dependencies,
        execpath, &execname, cli.verbose, ""
    ) {
        eprintln!("Failed to traverse dependencies: {}", e);
        std::process::exit(1);
    }

    if cli.verbose {
        eprintln!("\nNeed these libs: {:#?}\n", used_libs);
        eprintln!("{:#?}", dependencies);
    }

    let output_name = cli.output.unwrap_or("sharedpacker_out".into());
    if output_name.is_dir() && output_name.exists() && !cli.force {
        eprintln!("Output directory {:?} already exists. use --force if you want to override", output_name);
        std::process::exit(1);
    }

    // now iterate over the flat list of dependencies and copy all of them
    // to the output folder
    if let Err(e) = copy_dependencies_to_output_folder(
        &output_name, &dependencies
    ) {
        eprintln!("Failed to copy dependencies to output folder: {}", e);
        std::process::exit(1);
    }


    // // we also copy the original exec path given to us
    // let execname = match execpath.file_name() {
    //     Some(n) => n,
    //     None => {
    //         eprintln!("Failed to get exec path file name from {:?}", execpath);
    //         // cleanup_if_err(&output_name);
    //         std::process::exit(1);
    //     }
    // };
    // let mut output_path = output_name.clone();
    // output_path.push(execname);
    // if let Err(e) = std::fs::copy(execpath, &output_path) {
    //     eprintln!("Failed to copy {:?} to {:?}\n{}", execpath, output_path, e);
    //     // cleanup_if_err(&output_name);
    //     std::process::exit(1);
    // }

    // // then we make a wrapper script
    // let execnamestr = execname.to_string_lossy();
    // let wrapper_script = format!("#!/usr/bin/env bash\n\nLD_LIBRARY_PATH=. ./{}", execnamestr);
    // let mut wrapper_script_path = output_name.clone();
    // let wrapper_script_name = format!("{}_wrapped", execnamestr);
    // wrapper_script_path.push(wrapper_script_name);
    // if let Err(e) = std::fs::write(&wrapper_script_path, wrapper_script) {
    //     eprintln!("Failed to write wrapper script at {:?}\n{}", wrapper_script_path, e);
    //     // cleanup_if_err(&output_name);
    //     std::process::exit(1);
    // }

    // // too lazy to figure out how to do chmod +x in rust, so just do it as a command:
    // let exec_args = ["chmod", "+x", wrapper_script_path.as_os_str().to_str().unwrap()];
    // let res = exechelper::execute(&exec_args);
    // match res {
    //     Ok(status) => {
    //         if status.status != 0 {
    //             eprintln!("chmod +x {:?} failed", wrapper_script_path);
    //             eprintln!("{}", status.stderr);
    //             std::process::exit(1);
    //         }
    //     }
    //     Err(e) => {
    //         eprintln!("Failed to chmod +x wrapper script: {}", e);
    //         std::process::exit(1);
    //     }
    // }
}
