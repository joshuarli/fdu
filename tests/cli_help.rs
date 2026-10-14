use std::io;
use std::process::Command;

#[test]
fn help_matches_build_features() -> io::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_fdu")).arg("--help").output()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());

    let expected = if cfg!(feature = "interactive") {
        concat!(
            "Usage: fdu [--apparent] [-x PATH | --exclude PATH]... [--summary | --interactive] [--read-only] [PATH]\n",
            "       fdu --rmrf [--yes] PATH\n",
            "\n",
            "Browse a directory in the terminal, or print immediate child-directory totals.\n",
            "By default, sizes are allocated bytes; --apparent uses logical lengths.\n",
            "--summary forces output mode; --interactive requires a usable terminal.\n",
            "--read-only disables deletion in interactive mode.\n",
            "-x, --exclude PATH omits an exact path relative to the selected root; repeat as needed.\n",
            "--rmrf deletes indexed entries below PATH, keeping PATH itself, without a\n",
            "terminal interface. PATH is required. A symlink root is refused, even\n",
            "with trailing slashes. /, your home directory, and any parent of\n",
            "the current directory are refused. Incomplete selected directories and\n",
            "mount boundaries reject the plan. An incomplete root scan stops before\n",
            "any entries are removed.\n",
            "It asks for confirmation on a terminal; --yes skips that and is required\n",
            "when stdin is not a terminal.\n",
            "Exclusions are not accepted with --rmrf.\n",
        )
    } else {
        concat!(
            "Usage: fdu [--apparent] [-x PATH | --exclude PATH]... [--summary] [PATH]\n",
            "\n",
            "Print immediate child-directory totals.\n",
            "By default, sizes are allocated bytes; --apparent uses logical lengths.\n",
            "--summary selects summary output explicitly.\n",
            "-x, --exclude PATH omits an exact path relative to the selected root; repeat as needed.\n",
        )
    };
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    Ok(())
}
