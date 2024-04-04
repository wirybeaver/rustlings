use crate::embedded::{WriteStrategy, EMBEDDED_FILES};
use crate::exercise::{Exercise, ExerciseList};
use crate::run::run;
use crate::verify::verify;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use console::Emoji;
use notify_debouncer_mini::notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{io, thread};
use verify::VerifyState;

#[macro_use]
mod ui;

mod embedded;
mod exercise;
mod init;
mod run;
mod verify;

/// Rustlings is a collection of small exercises to get you used to writing and reading Rust code
#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Option<Subcommands>,
}

#[derive(Subcommand)]
enum Subcommands {
    /// Initialize Rustlings
    Init,
    /// Verify all exercises according to the recommended order
    Verify,
    /// Rerun `verify` when files were edited
    Watch,
    /// Run/Test a single exercise
    Run {
        /// The name of the exercise
        name: String,
    },
    /// Reset a single exercise
    Reset {
        /// The name of the exercise
        name: String,
    },
    /// Return a hint for the given exercise
    Hint {
        /// The name of the exercise
        name: String,
    },
    /// List the exercises available in Rustlings
    List {
        /// Show only the paths of the exercises
        #[arg(short, long)]
        paths: bool,
        /// Show only the names of the exercises
        #[arg(short, long)]
        names: bool,
        /// Provide a string to match exercise names.
        /// Comma separated patterns are accepted
        #[arg(short, long)]
        filter: Option<String>,
        /// Display only exercises not yet solved
        #[arg(short, long)]
        unsolved: bool,
        /// Display only exercises that have been solved
        #[arg(short, long)]
        solved: bool,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.command.is_none() {
        println!("\n{WELCOME}\n");
    }

    which::which("cargo").context(
        "Failed to find `cargo`.
Did you already install Rust?
Try running `cargo --version` to diagnose the problem.",
    )?;

    let exercises = ExerciseList::parse()?.exercises;

    if matches!(args.command, Some(Subcommands::Init)) {
        init::init_rustlings(&exercises).context("Initialization failed")?;
        println!(
            "\nDone initialization!\n
Run `cd rustlings` to go into the generated directory.
Then run `rustlings` for further instructions on getting started."
        );
        return Ok(());
    } else if !Path::new("exercises").is_dir() {
        println!(
            "\nThe `exercises` directory wasn't found in the current directory.
If you are just starting with Rustlings, run the command `rustlings init` to initialize it."
        );
        exit(1);
    }

    let command = args.command.unwrap_or_else(|| {
        println!("{DEFAULT_OUT}\n");
        exit(0);
    });

    match command {
        // `Init` is handled above.
        Subcommands::Init => (),
        Subcommands::List {
            paths,
            names,
            filter,
            unsolved,
            solved,
        } => {
            if !paths && !names {
                println!("{:<17}\t{:<46}\t{:<7}", "Name", "Path", "Status");
            }
            let mut exercises_done: u16 = 0;
            let lowercase_filter = filter
                .as_ref()
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let filters = lowercase_filter
                .split(',')
                .filter_map(|f| {
                    let f = f.trim();
                    if f.is_empty() {
                        None
                    } else {
                        Some(f)
                    }
                })
                .collect::<Vec<_>>();

            for exercise in &exercises {
                let fname = exercise.path.to_string_lossy();
                let filter_cond = filters
                    .iter()
                    .any(|f| exercise.name.contains(f) || fname.contains(f));
                let looks_done = exercise.looks_done()?;
                let status = if looks_done {
                    exercises_done += 1;
                    "Done"
                } else {
                    "Pending"
                };
                let solve_cond =
                    (looks_done && solved) || (!looks_done && unsolved) || (!solved && !unsolved);
                if solve_cond && (filter_cond || filter.is_none()) {
                    let line = if paths {
                        format!("{fname}\n")
                    } else if names {
                        format!("{}\n", exercise.name)
                    } else {
                        format!("{:<17}\t{fname:<46}\t{status:<7}\n", exercise.name)
                    };
                    // Somehow using println! leads to the binary panicking
                    // when its output is piped.
                    // So, we're handling a Broken Pipe error and exiting with 0 anyway
                    let stdout = std::io::stdout();
                    {
                        let mut handle = stdout.lock();
                        handle.write_all(line.as_bytes()).unwrap_or_else(|e| {
                            match e.kind() {
                                std::io::ErrorKind::BrokenPipe => exit(0),
                                _ => exit(1),
                            };
                        });
                    }
                }
            }

            let percentage_progress = exercises_done as f32 / exercises.len() as f32 * 100.0;
            println!(
                "Progress: You completed {} / {} exercises ({:.1} %).",
                exercises_done,
                exercises.len(),
                percentage_progress
            );
            exit(0);
        }

        Subcommands::Run { name } => {
            let exercise = find_exercise(&name, &exercises)?;
            run(exercise).unwrap_or_else(|_| exit(1));
        }

        Subcommands::Reset { name } => {
            let exercise = find_exercise(&name, &exercises)?;
            EMBEDDED_FILES
                .write_exercise_to_disk(&exercise.path, WriteStrategy::Overwrite)
                .with_context(|| format!("Failed to reset the exercise {exercise}"))?;
            println!("The file {} has been reset!", exercise.path.display());
        }

        Subcommands::Hint { name } => {
            let exercise = find_exercise(&name, &exercises)?;
            println!("{}", exercise.hint);
        }

        Subcommands::Verify => match verify(&exercises, (0, exercises.len()))? {
            VerifyState::AllExercisesDone => println!("All exercises done!"),
            VerifyState::Failed(exercise) => bail!("Exercise {exercise} failed"),
        },

        Subcommands::Watch => match watch(&exercises) {
            Err(e) => {
                println!("Error: Could not watch your progress. Error message was {e:?}.");
                println!("Most likely you've run out of disk space or your 'inotify limit' has been reached.");
                exit(1);
            }
            Ok(WatchStatus::Finished) => {
                println!(
                    "{emoji} All exercises completed! {emoji}",
                    emoji = Emoji("🎉", "★")
                );
                println!("\n{FENISH_LINE}\n");
            }
            Ok(WatchStatus::Unfinished) => {
                println!("We hope you're enjoying learning about Rust!");
                println!("If you want to continue working on the exercises at a later point, you can simply run `rustlings watch` again");
            }
        },
    }

    Ok(())
}

fn spawn_watch_shell(
    failed_exercise_hint: Arc<Mutex<Option<String>>>,
    should_quit: Arc<AtomicBool>,
) {
    println!("Welcome to watch mode! You can type 'help' to get an overview of the commands you can use here.");

    thread::spawn(move || {
        let mut input = String::with_capacity(32);
        let mut stdin = io::stdin().lock();

        loop {
            // Recycle input buffer.
            input.clear();

            if let Err(e) = stdin.read_line(&mut input) {
                println!("error reading command: {e}");
            }

            let input = input.trim();
            if input == "hint" {
                if let Some(hint) = &*failed_exercise_hint.lock().unwrap() {
                    println!("{hint}");
                }
            } else if input == "clear" {
                println!("\x1B[2J\x1B[1;1H");
            } else if input == "quit" {
                should_quit.store(true, Ordering::SeqCst);
                println!("Bye!");
            } else if input == "help" {
                println!("{WATCH_MODE_HELP_MESSAGE}");
            } else {
                println!("unknown command: {input}\n{WATCH_MODE_HELP_MESSAGE}");
            }
        }
    });
}

fn find_exercise<'a>(name: &str, exercises: &'a [Exercise]) -> Result<&'a Exercise> {
    if name == "next" {
        for exercise in exercises {
            if !exercise.looks_done()? {
                return Ok(exercise);
            }
        }

        println!("🎉 Congratulations! You have done all the exercises!");
        println!("🔚 There are no more exercises to do next!");
        exit(0);
    }

    exercises
        .iter()
        .find(|e| e.name == name)
        .with_context(|| format!("No exercise found for '{name}'!"))
}

enum WatchStatus {
    Finished,
    Unfinished,
}

fn watch(exercises: &[Exercise]) -> Result<WatchStatus> {
    /* Clears the terminal with an ANSI escape code.
    Works in UNIX and newer Windows terminals. */
    fn clear_screen() {
        println!("\x1Bc");
    }

    let (tx, rx) = channel();
    let should_quit = Arc::new(AtomicBool::new(false));

    let mut debouncer = new_debouncer(Duration::from_secs(1), tx)?;
    debouncer
        .watcher()
        .watch(Path::new("exercises"), RecursiveMode::Recursive)?;

    clear_screen();

    let failed_exercise_hint = match verify(exercises, (0, exercises.len()))? {
        VerifyState::AllExercisesDone => return Ok(WatchStatus::Finished),
        VerifyState::Failed(exercise) => Arc::new(Mutex::new(Some(exercise.hint.clone()))),
    };

    spawn_watch_shell(Arc::clone(&failed_exercise_hint), Arc::clone(&should_quit));

    let mut pending_exercises = Vec::with_capacity(exercises.len());
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => match event {
                Ok(events) => {
                    for event in events {
                        if event.kind == DebouncedEventKind::Any
                            && event.path.extension().is_some_and(|ext| ext == "rs")
                        {
                            pending_exercises.extend(exercises.iter().filter(|exercise| {
                                !exercise.looks_done().unwrap_or(false)
                                    || event.path.ends_with(&exercise.path)
                            }));
                            let num_done = exercises.len() - pending_exercises.len();

                            clear_screen();

                            match verify(
                                pending_exercises.iter().copied(),
                                (num_done, exercises.len()),
                            )? {
                                VerifyState::AllExercisesDone => return Ok(WatchStatus::Finished),
                                VerifyState::Failed(exercise) => {
                                    let hint = exercise.hint.clone();
                                    *failed_exercise_hint.lock().unwrap() = Some(hint);
                                }
                            }

                            pending_exercises.clear();
                        }
                    }
                }
                Err(e) => println!("watch error: {e:?}"),
            },
            Err(RecvTimeoutError::Timeout) => {
                // the timeout expired, just check the `should_quit` variable below then loop again
            }
            Err(e) => println!("watch error: {e:?}"),
        }
        // Check if we need to exit
        if should_quit.load(Ordering::SeqCst) {
            return Ok(WatchStatus::Unfinished);
        }
    }
}

const WELCOME: &str = r"       welcome to...
                 _   _ _
  _ __ _   _ ___| |_| (_)_ __   __ _ ___
 | '__| | | / __| __| | | '_ \ / _` / __|
 | |  | |_| \__ \ |_| | | | | | (_| \__ \
 |_|   \__,_|___/\__|_|_|_| |_|\__, |___/
                               |___/";

const DEFAULT_OUT: &str =
    "Is this your first time? Don't worry, Rustlings was made for beginners! We are
going to teach you a lot of things about Rust, but before we can get
started, here's a couple of notes about how Rustlings operates:

1. The central concept behind Rustlings is that you solve exercises. These
   exercises usually have some sort of syntax error in them, which will cause
   them to fail compilation or testing. Sometimes there's a logic error instead
   of a syntax error. No matter what error, it's your job to find it and fix it!
   You'll know when you fixed it because then, the exercise will compile and
   Rustlings will be able to move on to the next exercise.
2. If you run Rustlings in watch mode (which we recommend), it'll automatically
   start with the first exercise. Don't get confused by an error message popping
   up as soon as you run Rustlings! This is part of the exercise that you're
   supposed to solve, so open the exercise file in an editor and start your
   detective work!
3. If you're stuck on an exercise, there is a helpful hint you can view by typing
   'hint' (in watch mode), or running `rustlings hint exercise_name`.
4. If an exercise doesn't make sense to you, feel free to open an issue on GitHub!
   (https://github.com/rust-lang/rustlings/issues/new). We look at every issue,
   and sometimes, other learners do too so you can help each other out!

Got all that? Great! To get started, run `rustlings watch` in order to get the first exercise.
Make sure to have your editor open in the `rustlings` directory!";

const WATCH_MODE_HELP_MESSAGE: &str = "Commands available to you in watch mode:
  hint   - prints the current exercise's hint
  clear  - clears the screen
  quit   - quits watch mode
  help   - displays this help message

Watch mode automatically re-evaluates the current exercise
when you edit a file's contents.";

const FENISH_LINE: &str = "+----------------------------------------------------+
|          You made it to the Fe-nish line!          |
+--------------------------  ------------------------+
                           \\/\x1b[31m
     ▒▒          ▒▒▒▒▒▒▒▒      ▒▒▒▒▒▒▒▒          ▒▒
   ▒▒▒▒  ▒▒    ▒▒        ▒▒  ▒▒        ▒▒    ▒▒  ▒▒▒▒
   ▒▒▒▒  ▒▒  ▒▒            ▒▒            ▒▒  ▒▒  ▒▒▒▒
 ░░▒▒▒▒░░▒▒  ▒▒            ▒▒            ▒▒  ▒▒░░▒▒▒▒
   ▓▓▓▓▓▓▓▓  ▓▓      ▓▓██  ▓▓  ▓▓██      ▓▓  ▓▓▓▓▓▓▓▓
     ▒▒▒▒    ▒▒      ████  ▒▒  ████      ▒▒░░  ▒▒▒▒
       ▒▒  ▒▒▒▒▒▒        ▒▒▒▒▒▒        ▒▒▒▒▒▒  ▒▒
         ▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▒▒▒▒▒▒▒▒▓▓▒▒▓▓▒▒▒▒▒▒▒▒
           ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
             ▒▒▒▒▒▒▒▒▒▒██▒▒▒▒▒▒██▒▒▒▒▒▒▒▒▒▒
           ▒▒  ▒▒▒▒▒▒▒▒▒▒██████▒▒▒▒▒▒▒▒▒▒  ▒▒
         ▒▒    ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒    ▒▒
       ▒▒    ▒▒    ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒    ▒▒    ▒▒
       ▒▒  ▒▒    ▒▒                  ▒▒    ▒▒  ▒▒
           ▒▒  ▒▒                      ▒▒  ▒▒\x1b[0m

We hope you enjoyed learning about the various aspects of Rust!
If you noticed any issues, please don't hesitate to report them to our repo.
You can also contribute your own exercises to help the greater community!

Before reporting an issue or contributing, please read our guidelines:
https://github.com/rust-lang/rustlings/blob/main/CONTRIBUTING.md";
