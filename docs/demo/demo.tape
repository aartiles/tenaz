# The README's demo GIF: a scripted `tenaz demo` run, with no model and
# no agent. Render it with docs/demo/render.sh, which builds Tenaz, runs
# this tape and shrinks the GIF.

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set Framerate 8
Set PlaybackSpeed 2

# Use the binary built from this checkout, in an empty directory.
Hide
Type `export PATH="$PWD/target/release:$PATH" PS1='$ '; cd "$(mktemp -d)"; clear`
Enter
Show

Type "# scripted run: no model, no agent"
Sleep 1s
Enter
Type "tenaz demo --project demo"
Sleep 500ms
Enter
Sleep 26s

Hide
Type "clear"
Enter
Show
Type "tenaz status --project demo"
Sleep 500ms
Enter
Sleep 10s
