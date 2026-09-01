---
title: CLI Reference
description: "Reference for FZed's command-line interface (CLI), including opening files and directories, integrating with tools, and controlling FZed from scripts."
---

# CLI Reference

Use FZed's command-line interface (CLI) to open files and directories, integrate with tools, and control FZed from scripts.

## Installation

**macOS:** Run the {#action cli::InstallCliBinary} command from the command palette ({#kb command_palette::Toggle}) to install the `fzed` CLI to `/usr/local/bin/fzed`.

**Linux:** The CLI is included with FZed packages as `fzed`.

**Windows:** The CLI is included with FZed. Add FZed's installation directory to your PATH, or use the full path to `fzed.exe`.

## Usage

```sh
fzed [OPTIONS] [PATHS]...
```

## Opening Files and Directories

Open a file:

```sh
fzed myfile.txt
```

Open a directory as a workspace:

```sh
fzed ~/projects/myproject
```

Open multiple files or directories:

```sh
fzed file1.txt file2.txt ~/projects/myproject
```

Open a file at a specific line and column:

```sh
fzed myfile.txt:42        # Open at line 42
fzed myfile.txt:42:10     # Open at line 42, column 10
```

## Options

### `-w`, `--wait`

Wait for all opened files to be closed before the CLI exits. When opening a directory, waits until the window is closed.

This is useful for integrating FZed with tools that expect an editor to block until editing is complete (e.g., `git commit`):

```sh
export EDITOR="fzed --wait"
git commit  # Opens FZed and waits for you to close the commit message file
```

### `-n`, `--new`

Open paths in a new workspace window, even if the paths are already open in an existing window:

```sh
fzed -n ~/projects/myproject
```

### `-a`, `--add`

Add paths to the currently focused workspace instead of opening a new window. When multiple workspace windows are open, files open in the focused window:

```sh
fzed -a newfile.txt
```

### `-r`, `--reuse`

Reuse an existing window, replacing its current workspace with the new paths:

```sh
fzed -r ~/projects/different-project
```

### `-e`, `--existing`

Open paths in an existing FZed window instead of creating a new one:

```sh
fzed -e myfile.txt
```

By default (without `-n`, `-a`, `-r`, or `-e`), directories open in the current window's sidebar. You can change this default with the `cli_default_open_behavior` setting. See [Windows & Projects](../windows-and-projects.md) for more details.

### `--diff <OLD_PATH> <NEW_PATH>`

Open a diff view comparing two files. Can be specified multiple times:

```sh
fzed --diff file1.txt file2.txt
fzed --diff old.rs new.rs --diff old2.rs new2.rs
```

### `--foreground`

Run FZed in the foreground, keeping the terminal attached. Useful for debugging:

```sh
fzed --foreground
```

### `--user-data-dir <DIR>`

Use a custom directory for all user data (database, extensions, logs) instead of the default location:

```sh
fzed --user-data-dir ~/.fzed-custom
```

Default locations:

- **macOS:** `~/Library/Application Support/fzed`
- **Linux:** `$XDG_DATA_HOME/fzed` (typically `~/.local/share/fzed`)
- **Windows:** `%LOCALAPPDATA%\fzed`

### `-v`, `--version`

Print FZed's version and exit:

```sh
fzed --version
```

### `--completions <SHELL>`

Generate shell completions for the `fzed` CLI:

#### Bash

Add to `~/.bashrc`:

```bash
eval "$(fzed --completions bash)"
```

#### Elvish

Add to `~/.config/elvish/rc.elv`:

```elvish
set edit:completion:arg-completer[fzed] = { |@args|
    eval (fzed --completions elvish | slurp)
    $edit:completion:arg-completer[fzed] $@args
}
```

#### Fish

Add to `~/.config/fish/config.fish`:

```fish
fzed --completions fish | source
```

#### Nushell

Add to `~/.config/nushell/config.nu`:

```nu
mkdir ($nu.data-dir | path join "vendor/autoload")
^fzed --completions nushell | save --force ($nu.data-dir | path join "vendor/autoload/fzed.nu")
```

#### Powershell

Add to `$PROFILE`:

```powershell
(&fzed --completions powershell) | Out-String | Invoke-Expression
```

#### Zsh

Add to `~/.zshrc`:

```zsh
eval "$(fzed --completions zsh)"
```

### `--uninstall`

Uninstall FZed and remove all related files (macOS and Linux only):

```sh
fzed --uninstall
```

### `--zed <PATH>`

Specify a custom path to the FZed application or binary:

```sh
fzed --zed /path/to/FZed.app myfile.txt
```

## Reading from Standard Input

Read content from stdin by passing `-` as the path:

```sh
echo "Hello, World!" | fzed -
cat myfile.txt | fzed -
ps aux | fzed -
```

This creates a temporary file with the stdin content and opens it in FZed.

## URL Handling

The CLI can open `fzed://`, `file://`, and `ssh://` URLs:

```sh
fzed fzed://settings
fzed file:///Users/whatever/.zshrc
fzed ssh://me@example.com/abs/path
fzed ssh://me@example.com:/abs/path
fzed ssh://me@example.com/~/project
fzed ssh://me@example.com:~/project
```

## Using FZed as Your Default Editor

Set FZed as your default editor for Git and other tools:

```sh
export EDITOR="fzed --wait"
export VISUAL="fzed --wait"
```

Add these lines to your shell configuration file (e.g., `~/.bashrc`, `~/.zshrc`).

## macOS: Switching Release Channels

On macOS, you can launch a specific release channel by passing the channel name as the first argument:

```sh
fzed --stable myfile.txt
fzed --preview myfile.txt
fzed --nightly myfile.txt
```

## WSL Integration (Windows)

On Windows, the CLI supports opening paths from WSL distributions. This is handled automatically when launching FZed from within WSL.

## Exit Codes

| Code | Meaning                           |
| ---- | --------------------------------- |
| `0`  | Success                           |
| `1`  | Error (details printed to stderr) |

When using `--wait`, the exit code reflects whether the files were saved before closing.
