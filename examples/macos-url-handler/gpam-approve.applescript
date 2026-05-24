-- macOS URL handler for GCP PAM approval links.
-- Receives https:// URLs from Launch Services and forwards the embedded grant
-- resource name to a running gpam TUI via `gpam send`.
-- Build into Gpam Approve.app with: ./build.sh

on open location this_URL
    try
        do shell script "echo \"[gpam] $(date) " & quoted form of this_URL & "\" >> /tmp/gpam-handler.log"
    end try

    try
        set tmpPath to do shell script "mktemp /tmp/gpam-approve.XXXXXX.command"
        set lf to linefeed
        set safeURL to my shellSingleQuote(this_URL)
        set scriptBody to "#!/bin/zsh" & lf & ¬
            "trap 'rm -f \"$0\"' EXIT" & lf & ¬
            "URL=" & safeURL & lf & ¬
            "raw=\"${URL#*;grantId=}\"" & lf & ¬
            "[ \"$raw\" = \"$URL\" ] && { echo 'no grantId in URL' >&2; exit 2; }" & lf & ¬
            "raw=\"${raw%%[;?]*}\"" & lf & ¬
            "name=$(/usr/bin/env python3 -c 'import sys,urllib.parse; sys.stdout.write(urllib.parse.unquote(sys.argv[1]))' \"$raw\")" & lf & ¬
            "gpam send \"$name\" || gpam approve \"$name\"" & lf
        do shell script "cat > " & quoted form of tmpPath & " <<'GPAM_END'" & lf & scriptBody & "GPAM_END"
        do shell script "chmod +x " & quoted form of tmpPath
        -- open honors the user's .command handler (Terminal by default) without
        -- needing Automation permission or spawning a second window.
        do shell script "open " & quoted form of tmpPath
    on error errMsg number errNum
        try
            do shell script "echo " & quoted form of ("[gpam error] " & errNum & ": " & errMsg) & " >> /tmp/gpam-handler.log"
        end try
    end try
end open location

on run
    try
        do shell script "echo \"[gpam] $(date) launched with no URL\" >> /tmp/gpam-handler.log"
    end try
end run

-- Single-quote a string for safe shell embedding.
-- Closes the quote, inserts an escaped quote, then reopens: the standard
-- portable idiom for arbitrary content in single-quoted shell arguments.
on shellSingleQuote(s)
    set AppleScript's text item delimiters to "'"
    set parts to text items of s
    set AppleScript's text item delimiters to "'\\''"
    set escaped to parts as text
    set AppleScript's text item delimiters to ""
    return "'" & escaped & "'"
end shellSingleQuote
