//! Create a standalone native executable for a given Wasm file.

use super::ObjectFormat;
use crate::store::CompilerOptions;
use anyhow::{Context, Result};
#[cfg(feature = "pirita_file")]
use pirita::{ParseOptions, PiritaFileMmap};
use std::env;
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use structopt::StructOpt;
use wasmer::*;
use wasmer_object::{emit_serialized, get_object_for_target};

const WASMER_MAIN_C_SOURCE: &str = include_str!("./wasmer_create_exe_main.c");
#[cfg(feature = "static-artifact-create")]
const WASMER_STATIC_MAIN_C_SOURCE: &[u8] = include_bytes!("./wasmer_static_create_exe_main.c");

#[derive(Debug, StructOpt)]
/// The options for the `wasmer create-exe` subcommand
pub struct CreateExe {
    /// Input file
    #[structopt(name = "FILE", parse(from_os_str))]
    path: PathBuf,

    /// Output file
    #[structopt(name = "OUTPUT PATH", short = "o", parse(from_os_str))]
    output: PathBuf,

    /// Compilation Target triple
    #[structopt(long = "target")]
    target_triple: Option<Triple>,

    /// Object format options
    ///
    /// This flag accepts two options: `symbols` or `serialized`.
    /// - (default) `symbols` creates an
    /// executable where all functions and metadata of the module are regular object symbols
    /// - `serialized` creates an executable where the module is zero-copy serialized as raw data
    #[structopt(name = "OBJECT_FORMAT", long = "object-format", verbatim_doc_comment)]
    object_format: Option<ObjectFormat>,

    /// Header file for object input
    ///
    /// If given, the input `PATH` is assumed to be an object created with `wasmer create-obj` and
    /// this is its accompanying header file.
    #[structopt(name = "HEADER", long = "header", verbatim_doc_comment)]
    header: Option<PathBuf>,

    #[structopt(short = "m", multiple = true, number_of_values = 1)]
    cpu_features: Vec<CpuFeature>,

    /// Additional libraries to link against.
    /// This is useful for fixing linker errors that may occur on some systems.
    #[structopt(short = "l", multiple = true, number_of_values = 1)]
    libraries: Vec<String>,

    #[structopt(flatten)]
    compiler: CompilerOptions,
}

impl CreateExe {
    /// Runs logic for the `compile` subcommand
    pub fn execute(&self) -> Result<()> {
        let target = self
            .target_triple
            .as_ref()
            .map(|target_triple| {
                let mut features = self
                    .cpu_features
                    .clone()
                    .into_iter()
                    .fold(CpuFeature::set(), |a, b| a | b);
                // Cranelift requires SSE2, so we have this "hack" for now to facilitate
                // usage
                features |= CpuFeature::SSE2;
                Target::new(target_triple.clone(), features)
            })
            .unwrap_or_default();

        let starting_cd = env::current_dir()?;
        let wasm_module_path = starting_cd.join(&self.path);
        #[cfg(feature = "pirita_file")]
        {
            if let Ok(pirita) =
                PiritaFileMmap::parse(wasm_module_path.clone(), &ParseOptions::default())
            {
                return self.create_exe_pirita(&pirita, target);
            }
        }

        let (store, compiler_type) = self.compiler.get_store_for_target(target.clone())?;
        let object_format = self.object_format.unwrap_or(ObjectFormat::Symbols);

        println!("Compiler: {}", compiler_type.to_string());
        println!("Target: {}", target.triple());
        println!("Format: {:?}", object_format);

        let working_dir = tempfile::tempdir()?;
        let working_dir = working_dir.path().to_path_buf();
        let output_path = starting_cd.join(&self.output);

        #[cfg(not(windows))]
        let wasm_object_path = working_dir.clone().join("wasm.o");
        #[cfg(windows)]
        let wasm_object_path = working_dir.clone().join("wasm.obj");

        match object_format {
            ObjectFormat::Serialized => {
                let module = Module::from_file(&store, &wasm_module_path);
                let module = module.context("failed to compile Wasm")?;
                let bytes = module.serialize()?;
                let mut obj = get_object_for_target(target.triple())?;
                emit_serialized(&mut obj, &bytes, target.triple(), "WASMER_MODULE")?;
                let mut writer = BufWriter::new(File::create(&wasm_object_path)?);
                obj.write_stream(&mut writer)
                    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
                writer.flush()?;
                drop(writer);
        
                self.compile_c(wasm_object_path, output_path)?;
            }
            #[cfg(not(feature = "static-artifact-create"))]
            ObjectFormat::Symbols => {
                return Err(anyhow!("This version of wasmer-cli hasn't been compiled with static artifact support. You need to enable the `static-artifact-create` feature during compilation."));
            }
            #[cfg(feature = "static-artifact-create")]
            ObjectFormat::Symbols => {
                let engine = store.engine();
                let engine_inner = engine.inner();
                let compiler = engine_inner.compiler()?;
                let features = engine_inner.features();
                let tunables = store.tunables();
                let data: Vec<u8> = fs::read(wasm_module_path)?;
                let prefixer: Option<Box<dyn Fn(&[u8]) -> String + Send>> = None;
                let (module_info, obj, metadata_length, symbol_registry) =
                    Artifact::generate_object(
                        compiler, &data, prefixer, &target, tunables, features,
                    )?;

                let header_file_src = crate::c_gen::staticlib_header::generate_header_file(
                    &module_info,
                    &*symbol_registry,
                    metadata_length,
                );
                /* Write object file with functions */
                let object_file_path: std::path::PathBuf =
                    std::path::Path::new("functions.o").into();
                let mut writer = BufWriter::new(File::create(&object_file_path)?);
                obj.write_stream(&mut writer)
                    .map_err(|err| anyhow::anyhow!(err.to_string()))?;
                writer.flush()?;
                /* Write down header file that includes pointer arrays and the deserialize function
                 * */
                 println!("header_file_src:\r\n{header_file_src}");
                let mut writer = BufWriter::new(File::create("static_defs.h")?);
                writer.write_all(header_file_src.as_bytes())?;
                writer.flush()?;
                link(
                    output_path,
                    object_file_path,
                    std::path::Path::new("static_defs.h").into(),
                )?;
            }
        }

        eprintln!(
            "✔ Native executable compiled successfully to `{}`.",
            self.output.display(),
        );

        Ok(())
    }

    fn generate_run_code(module_name: &str) -> String {
        static CREATE_INSTANCE_CODE: &str = include_str!("./wasmer_create_exe_create_instance.c");
        CREATE_INSTANCE_CODE.replace("module,", &format!("{module_name},"))
    }

    #[cfg(feature = "pirita_file")]
    fn create_exe_pirita(&self, file: &PiritaFileMmap, target: Target) -> anyhow::Result<()> {
        use wasmer_object::emit_data;

        let starting_cd = env::current_dir()?;
        let working_dir = tempfile::tempdir()?;
        let working_dir = working_dir.path().to_path_buf();
        let output_path = starting_cd.join(&self.output);

        let volume_bytes = file.get_volumes_as_fileblock();
        let mut volumes_object = get_object_for_target(&target.triple())?;
        emit_data(&mut volumes_object, b"VOLUMES", volume_bytes.as_slice(), 1)?;

        let mut link_objects = Vec::new();

        #[cfg(not(windows))]
        let volume_object_path = working_dir.clone().join("volumes.o");
        #[cfg(windows)]
        let volume_object_path = working_dir.clone().join("volumes.obj");

        let (store, _) = self.compiler.get_store_for_target(target.clone())?;

        let mut c_code_to_add = format!("
    
        extern size_t VOLUMES_LENGTH asm(\"VOLUMES_LENGTH\");
        extern char VOLUMES_DATA asm(\"VOLUMES_DATA\");

        int init_filesystem_from_binary(wasm_vec_t* filesystem) {
            return 0;
        }
        ");
        let mut c_code_to_instantiate = String::new();
        let mut deallocate_module = String::new();

        let atom_to_run = match file.manifest.entrypoint.as_ref() {
            Some(s) => { 
                file.get_atom_name_for_command("wasi", s)
                .map_err(|e| anyhow!("Could not get atom for entrypoint: {e}"))? 
            },
            None => { return Err(anyhow!("Cannot compile to exe: no entrypoint to run package with")); },
        };

        let compiled_modules = file
            .get_all_atoms()
            .into_iter()
            .map(|(atom_name, atom_bytes)| {
                let module = Module::new(&store, &atom_bytes)
                    .context(format!("Failed to compile atom {atom_name:?} to wasm"))?;
                let bytes = module.serialize()?;
                let mut obj = get_object_for_target(target.triple())?;
                let atom_name_uppercase = atom_name.to_uppercase();
                emit_serialized(&mut obj, &bytes, target.triple(), &atom_name_uppercase)?;
                
                c_code_to_add.push_str(&format!("
                extern size_t {atom_name_uppercase}_LENGTH asm(\"{atom_name_uppercase}_LENGTH\");
                extern char {atom_name_uppercase}_DATA asm(\"{atom_name_uppercase}_DATA\");
                "));

                c_code_to_instantiate.push_str(&format!("
                wasm_byte_vec_t atom_{atom_name}_byte_vec = {{
                    .size = {atom_name_uppercase}_LENGTH,
                    .data = (const char*)&{atom_name_uppercase}_DATA,
                }};
                wasm_module_t *atom_{atom_name} = wasm_module_deserialize(store, &atom_{atom_name}_byte_vec);
    
                if (!atom_{atom_name}) {{
                    fprintf(stderr, \"Failed to create module from atom \\\"{atom_name}\\\"\\n\");
                    print_wasmer_error();
                    return -1;
                }}

                "));
                deallocate_module.push_str(&format!("wasm_module_delete(atom_{atom_name});"));
                Ok((atom_name.clone(), obj))
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;

        let mut writer = BufWriter::new(File::create(&volume_object_path)?);
        volumes_object
            .write_stream(&mut writer)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        writer.flush()?;
        drop(writer);

        link_objects.push(volume_object_path.clone());

        for (name, obj) in compiled_modules {
            #[cfg(not(windows))]
            let object_path = working_dir.clone().join(&format!("{name}.o"));
            #[cfg(windows)]
            let object_path = working_dir.clone().join(&format!("{name}.obj"));

            let mut writer = BufWriter::new(File::create(&object_path)?);
            obj.write_stream(&mut writer)
                .map_err(|err| anyhow::anyhow!(err.to_string()))?;
            writer.flush()?;
            drop(writer);

            link_objects.push(object_path.clone());
        }

        // write C src to disk
        let c_src_path = working_dir.clone().join("wasmer_main.c");
        #[cfg(not(windows))]
        let c_src_obj = working_dir.clone().join("wasmer_main.o");
        #[cfg(windows)]
        let c_src_obj = working_dir.clone().join("wasmer_main.obj");

        let run_code = Self::generate_run_code(&format!("atom_{atom_to_run}"));
        let c_code = WASMER_MAIN_C_SOURCE
            .replace("// DECLARE_MODULE", &c_code_to_add)
            .replace("// INSTANTIATE_MODULE", &c_code_to_instantiate)
            .replace("// DEALLOCATE_MODULE", &deallocate_module)
            .replace("// wasmer_create_exe_create_instance.c", &run_code);
        
        println!("before write pirita");

        std::fs::write(&c_src_path, c_code.as_bytes())
            .context("Failed to open C source code file")?;
        
        println!("after write pirita");

        run_c_compile(c_src_path.as_path(), &c_src_obj, self.target_triple.clone())
            .context("Failed to compile C source code")?;

        link_objects.push(c_src_obj.clone());

        println!("linking objects: {link_objects:#?}");

        LinkCode {
            object_paths: link_objects,
            output_path,
            additional_libraries: self.libraries.clone(),
            target: self.target_triple.clone(),
            ..Default::default()
        }
        .run()
        .context("Failed to link objects together")?;

        Ok(())
    }

    fn compile_c(&self, wasm_object_path: PathBuf, output_path: PathBuf) -> anyhow::Result<()> {
        // write C src to disk
        let c_src_path = Path::new("wasmer_main.c");
        #[cfg(not(windows))]
        let c_src_obj = PathBuf::from("wasmer_main.o");
        #[cfg(windows)]
        let c_src_obj = PathBuf::from("wasmer_main.obj");

        println!("before write");
        std::fs::write(
            &c_src_path, 
            WASMER_MAIN_C_SOURCE
            .replace("// DECLARE_MODULE", r#"
                extern size_t WASMER_MODULE_LENGTH asm("WASMER_MODULE_LENGTH");
                extern char WASMER_MODULE_DATA asm("WASMER_MODULE_DATA");
            "#)
            .replace(
                "// INSTANTIATE_MODULE",
                r#"
                wasm_byte_vec_t module_byte_vec = {
                    .size = WASMER_MODULE_LENGTH,
                    .data = (const char*)&WASMER_MODULE_DATA,
                  };
                  wasm_module_t *module = wasm_module_deserialize(store, &module_byte_vec);
                
                  if (!module) {
                    fprintf(stderr, "Failed to create module\n");
                    print_wasmer_error();
                    return -1;
                  }
                "#
            )
            .replace("// DEALLOCATE_MODULE", "wasm_module_delete(module);")
            .replace("// wasmer_create_exe_create_instance.c", &Self::generate_run_code("module"))
        )?;
        println!("after write");

        run_c_compile(c_src_path, &c_src_obj, self.target_triple.clone())
            .context("Failed to compile C source code")?;
        
        LinkCode {
            object_paths: vec![c_src_obj, wasm_object_path],
            output_path,
            additional_libraries: self.libraries.clone(),
            target: self.target_triple.clone(),
            ..Default::default()
        }
        .run()
        .context("Failed to link objects together")?;

        Ok(())
    }
}

#[cfg(feature = "static-artifact-create")]
fn link(
    output_path: PathBuf,
    object_path: PathBuf,
    mut header_code_path: PathBuf,
) -> anyhow::Result<()> {
    let linkcode = LinkCode {
        object_paths: vec![object_path, "main_obj.obj".into()],
        output_path,
        ..Default::default()
    };
    let c_src_path = Path::new("wasmer_main.c");
    let mut libwasmer_path = get_libwasmer_path()?
        .canonicalize()
        .context("Failed to find libwasmer")?;
    println!("Using libwasmer: {}", libwasmer_path.display());
    let _lib_filename = libwasmer_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    libwasmer_path.pop();
    println!("static artifact write");
    std::fs::write(&c_src_path, WASMER_STATIC_MAIN_C_SOURCE)
    .context("Failed to open C source code file")?;
    println!("static artifact after write, header_code_path = {:?}", header_code_path.canonicalize().unwrap().display());

    if !header_code_path.is_dir() {
        header_code_path.pop();
    }

    /* Compile main function */
    let compilation = Command::new("cc")
        .arg("-c")
        .arg(&c_src_path)
        .arg(if linkcode.optimization_flag.is_empty() {
            "-O2"
        } else {
            linkcode.optimization_flag.as_str()
        })
        .arg(&format!("-L{}", libwasmer_path.display()))
        .arg(&format!("-I{}", get_wasmer_include_directory()?.display()))
        //.arg(&format!("-l:{}", lib_filename))
        .arg("-lwasmer")
        // Add libraries required per platform.
        // We need userenv, sockets (Ws2_32), advapi32 for some system calls and bcrypt for random numbers.
        //#[cfg(windows)]
        //    .arg("-luserenv")
        //    .arg("-lWs2_32")
        //    .arg("-ladvapi32")
        //    .arg("-lbcrypt")
        // On unix we need dlopen-related symbols, libmath for a few things, and pthreads.
        //#[cfg(not(windows))]
        .arg("-ldl")
        .arg("-lm")
        .arg("-pthread")
        .arg(&format!("-I{}", header_code_path.display()))
        .arg("-v")
        .arg("-o")
        .arg("main_obj.obj")
        .output()?;
    if !compilation.status.success() {
        return Err(anyhow::anyhow!(String::from_utf8_lossy(
            &compilation.stderr
        )
        .to_string()));
    }
    linkcode.run().context("Failed to link objects together")?;
    Ok(())
}

fn get_wasmer_dir() -> anyhow::Result<PathBuf> {
    let wasmer_dir = PathBuf::from(
        env::var("WASMER_DIR")
            .or_else(|e| {
                option_env!("WASMER_INSTALL_PREFIX")
                    .map(str::to_string)
                    .ok_or(e)
            })
            .context("Trying to read env var `WASMER_DIR`")?,
    );
    let wasmer_dir = wasmer_dir.clone().canonicalize().unwrap_or(wasmer_dir);
    println!("wasmer dir = {:?}", wasmer_dir);
    Ok(wasmer_dir)
}

fn get_wasmer_include_directory() -> anyhow::Result<PathBuf> {
    let mut path = get_wasmer_dir()?;
    if path.clone().join("wasmer.h").exists() {
        return Ok(path);
    }
    path.push("include");
    if !path.clone().join("wasmer.h").exists() {
        println!("wasmer.h does not exist in {}, will probably default to the system path", path.canonicalize().unwrap().display());
    }

    Ok(path)
}

/// path to the static libwasmer
fn get_libwasmer_path() -> anyhow::Result<PathBuf> {
    let path = get_wasmer_dir()?;

    // TODO: prefer headless Wasmer if/when it's a separate library.
    #[cfg(not(windows))]
    let libwasmer_static_name = "libwasmer.a";
    #[cfg(windows)]
    let libwasmer_static_name = "libwasmer.lib";
    
    if path.exists() && path.join(libwasmer_static_name).exists() {
        Ok(path.join(libwasmer_static_name))
    } else {
        Ok(path.join("lib").join(libwasmer_static_name))
    }
}

/// Compile the C code.
fn run_c_compile(
    path_to_c_src: &Path,
    output_name: &Path,
    target: Option<Triple>,
) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    let c_compiler = "cc";
    // We must use a C++ compiler on Windows because wasm.h uses `static_assert`
    // which isn't available in `clang` on Windows.
    #[cfg(windows)]
    let c_compiler = "clang++";

    let mut command = Command::new(c_compiler);
    let command = command
        .arg("-O2")
        .arg("-c")
        .arg(path_to_c_src)
        .arg("-I")
        .arg(get_wasmer_include_directory()?);

    let command = if let Some(target) = target {
        command.arg("-target").arg(format!("{}", target))
    } else {
        command
    };

    let output = command.arg("-o").arg(output_name).output()?;

    if !output.status.success() {
        bail!(
            "C code compile failed with: stdout: {}\n\nstderr: {}",
            std::str::from_utf8(&output.stdout)
                .expect("stdout is not utf8! need to handle arbitrary bytes"),
            std::str::from_utf8(&output.stderr)
                .expect("stderr is not utf8! need to handle arbitrary bytes")
        );
    }
    Ok(())
}

/// Data used to run a linking command for generated artifacts.
#[derive(Debug)]
struct LinkCode {
    /// Path to the linker used to run the linking command.
    linker_path: PathBuf,
    /// String used as an optimization flag.
    optimization_flag: String,
    /// Paths of objects to link.
    object_paths: Vec<PathBuf>,
    /// Additional libraries to link against.
    additional_libraries: Vec<String>,
    /// Path to the output target.
    output_path: PathBuf,
    /// Path to the dir containing the static libwasmer library.
    libwasmer_path: PathBuf,
    /// The target to link the executable for.
    target: Option<Triple>,
}

impl Default for LinkCode {
    fn default() -> Self {
        #[cfg(not(windows))]
        let linker = "cc";
        #[cfg(windows)]
        let linker = "clang";
        Self {
            linker_path: PathBuf::from(linker),
            optimization_flag: String::from("-O2"),
            object_paths: vec![],
            additional_libraries: vec![],
            output_path: PathBuf::from("a.out"),
            libwasmer_path: get_libwasmer_path().unwrap(),
            target: None,
        }
    }
}

impl LinkCode {
    fn run(&self) -> anyhow::Result<()> {
        println!("LinkCode = {:?}", self.libwasmer_path);
        let libwasmer_path = self
            .libwasmer_path
            .clone()
            .canonicalize()
            .unwrap_or(self.libwasmer_path.clone());
        println!(
            "Using path `{}` as libwasmer path.",
            libwasmer_path.display()
        );
        let mut command = Command::new(&self.linker_path);
        let command = command
            .arg(&self.optimization_flag)
            .args(
                self.object_paths
                    .iter()
                    .map(|path| path.canonicalize().unwrap()),
            )
            .arg(&libwasmer_path);
        let command = if let Some(target) = &self.target {
            command.arg("-target").arg(format!("{}", target))
        } else {
            command
        };
        // Add libraries required per platform.
        // We need userenv, sockets (Ws2_32), advapi32 for some system calls and bcrypt for random numbers.
        #[cfg(windows)]
        let command = command
            .arg("-luserenv")
            .arg("-lWs2_32")
            .arg("-ladvapi32")
            .arg("-lbcrypt");
        // On unix we need dlopen-related symbols, libmath for a few things, and pthreads.
        #[cfg(not(windows))]
        let command = command.arg("-ldl").arg("-lm").arg("-pthread");
        let link_against_extra_libs = self
            .additional_libraries
            .iter()
            .map(|lib| format!("-l{}", lib));
        let command = command.args(link_against_extra_libs);
        let output = command.arg("-o").arg(&self.output_path).output()?;

        if !output.status.success() {
            bail!(
                "linking failed with: stdout: {}\n\nstderr: {}",
                std::str::from_utf8(&output.stdout)
                    .expect("stdout is not utf8! need to handle arbitrary bytes"),
                std::str::from_utf8(&output.stderr)
                    .expect("stderr is not utf8! need to handle arbitrary bytes")
            );
        }
        Ok(())
    }
}
