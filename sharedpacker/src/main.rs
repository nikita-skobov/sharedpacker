use exechelper;
use gumdrop::Options;
use std::{path::{Path, PathBuf}, collections::HashMap};

#[derive(Debug, Options)]
pub struct Cli {
    /// prints the help
    pub help: bool,

    /// print detailed logging info to stderr
    pub verbose: bool,

    /// name of folder to be created that will contain the archive of all of the shared libs
    #[options(short = "o")]
    pub output: Option<PathBuf>,

    /// if the output archive already exists by default we exit with an error and a message. if you pass the --force flag, we will override it
    #[options(short = "f")]
    pub force: bool,

    /// whatever the executable is, wrap it in a shell script that calls the executable with the correct LD_LIBRARY_PATH for you
    pub make_wrapper: bool,

    #[options(free)]
    pub exepath: Vec<PathBuf>
}

#[derive(Debug, Clone)]
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

pub fn parse_ldd_output(
    path: &Path,
    only_loader: bool,
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

        // if we are not considering the loader, then ignore when path starts with /
        // which i assume only happens for the loader?
        if !only_loader && libname.starts_with('/') {
            continue;
        }

        // if we are only interested in finding the loader
        // and we see that the libname starts with the /
        // then parse out the loader name
        let is_loader = only_loader && libname.starts_with('/');
        let libname = if is_loader {
            // if this is the loader it will usually start with /
            // so we want to remove its base bath and just have the file name
            libname.rsplit('/').next().unwrap_or(libname)
        } else { libname };

        let pathpart = match pathpart.find(' ') {
            None => pathpart,
            Some(index) => {
                &pathpart[0..index]
            }
        };

        // if we are only interested in the loader
        // and this one is the loader, then instead of outputting to the vec
        // just return here because we found it
        if is_loader {
            return Ok(vec![SharedLib {
                name: libname.into(),
                path: pathpart.into(),
            }]);
        }

        outvec.push(SharedLib {
            name: libname.into(),
            path: pathpart.into(),
        });
    }

    Ok(outvec)
}

pub fn get_lib_path_list(
    path: &Path,
) -> Result<Vec<SharedLib>, String> {
    parse_ldd_output(path, false)
}

pub fn get_loader(
    path: &Path,
) -> Result<SharedLib, String> {
    let loader = parse_ldd_output(path, true)?;
    match loader.get(0) {
        Some(lib) => Ok(lib.clone()),
        None => Err(format!("Failed to get loader from {:?}", path)),
    }
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

pub fn make_shell_script_wrapper(
    execname: &str,
    loadername: &str,
) -> String {
    // https://stackoverflow.com/a/4774063
    let part_one: String = "#!/usr/bin/env bash\n\nSCRIPTPATH=\"$( cd -- \"$(dirname \"$0\")\" >/dev/null 2>&1 ; pwd -P )\"".into();
    let part_two = format!("\"$SCRIPTPATH/{}\" --library-path \"$SCRIPTPATH\" \"$SCRIPTPATH/{}\" \"$@\"", loadername, execname);
    let out = format!("{}\n{}", part_one, part_two);
    out
}

pub fn copy_dependencies_to_output_folder(
    archive_path: &PathBuf,
    dependencies: &Vec<DependencyNode>,
    loader: &SharedLib,
    execname: &str,
    make_wrapper: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(&archive_path).map_err(|e| e.to_string())?;

    for dep in dependencies {
        let dep_path = &dep.path;
        let filename = dep_path.file_name()
            .map_or_else(|| Err(format!("Failed to find file name for {:?}", dep_path)), |o| Ok(o))?;
        let mut output_path = archive_path.clone();
        output_path.push(filename);

        std::fs::copy(dep_path, &output_path)
            .map_err(|e| format!("Failed to copy {:?} to {:?}\n{}", dep_path, output_path, e))?;

        // now change the loader to point to the specific one we copied
        patch_loader(&loader.name, &output_path)?;
    }

    // finally, copy the loader itself
    let mut new_loader_path = archive_path.clone();
    new_loader_path.push(loader.name.clone());
    std::fs::copy(&loader.path, &new_loader_path)
        .map_err(|e| format!("Failed to copy loader {:?} to {:?}\n{}", loader.path, new_loader_path, e))?;

    // also, if user wants to make a wrapper, we replace the archive_path/execname
    // with archive_path/.execname-original and make archive_path/execname a shell script
    // that launches archive_path/.execname-original with the correct LD_LIBRARY_PATH
    let mut old_exec = archive_path.clone();
    old_exec.push(execname);
    let mut new_exec = archive_path.clone();
    let newname = format!(".{}-original", execname);
    new_exec.push(&newname);
    if make_wrapper {
        std::fs::rename(&old_exec, &new_exec)
            .map_err(|e| format!("Failed to rename {:?} to {:?}\n{}", old_exec, new_exec, e))?;
        // now make the shell script
        let wrapper = make_shell_script_wrapper(&newname, &loader.name);
        std::fs::write(&old_exec, wrapper)
            .map_err(|e| e.to_string())?;
        // also make it executable:
        let old_exec_path = old_exec.to_string_lossy();
        let exec_args = ["chmod", "+x", &old_exec_path];
        match exechelper::execute(&exec_args) {
            Ok(out) => if out.status != 0 { return Err(out.stderr) },
            Err(e) => { return Err(e.to_string()) },
        }
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

    let loader = match get_loader(&execpath) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // now iterate over the flat list of dependencies and copy all of them
    // to the output folder
    if let Err(e) = copy_dependencies_to_output_folder(
        &output_name, &dependencies, &loader, &execname, cli.make_wrapper,
    ) {
        eprintln!("Failed to copy dependencies to output folder: {}", e);
        std::process::exit(1);
    }
}
