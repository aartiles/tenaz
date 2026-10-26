# The README's demo GIF: a scripted `tenaz demo` run, with no model and
# no agent. Render it with docs/demo/render.sh, which builds Tenaz, runs
# this tape and shrinks the GIF. Output goes through tidy.pl, which drops
# log timestamps, temp paths and the engine and memory lines of status.

Set Shell bash
Set FontSize 14
Set Width 1280
Set Height 720
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set Framerate 12
Set PlaybackSpeed 2

# Use the binary built from this checkout, in an empty directory.
Hide
Type `export PATH="$PWD/target/release:$PATH" PS1='$ ' DEMO_COLUMNS=134 TIDY="$PWD/docs/demo/tidy.pl"`
Enter
Type `cd "$(mktemp -d)" && export DEMO_DIR="$(pwd -P)"; tenaz() { command tenaz "$@" 2>&1 | perl "$TIDY"; }; clear`
Enter
Show

Type "# scripted run: no model, no agent"
Sleep 1s
Enter
Type "tenaz demo --project demo"
Sleep 500ms
Enter
Sleep 25s

Type "tenaz status --project demo"
Sleep 500ms
Enter
Sleep 8s
