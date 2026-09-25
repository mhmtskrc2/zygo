# The README's demo: serve once, exec twice, and the same request through a
# fresh sandbox for contrast. Recorded with VHS (https://github.com/charmbracelet/vhs)
# on a Linux host where `zygo doctor` is green and python:3.12-slim is pulled:
#
#     vhs packaging/demo/zygo.tape      # writes packaging/demo/zygo.gif
#
# The timings on screen are whatever the recording machine does; name the
# machine in the commit that adds the GIF.

Output packaging/demo/zygo.gif
Set Shell "bash"
Set FontSize 18
Set Width 1100
Set Height 560
Set TypingSpeed 40ms
Set Theme "Dracula"

Hide
Type `cd "$(mktemp -d)" && printf '%s\n' 'import hashlib, json' '' 'def handler(event):' '    return {"sha": hashlib.sha256(json.dumps(event).encode()).hexdigest()[:12]}' > handler.py && clear`
Enter
Show

Type `cat handler.py`
Enter
Sleep 1.5s

Type `zygo serve ./handler.py --name demo`
Enter
Sleep 2s

Type `time zygo exec demo '{"n": 1}'`
Enter
Sleep 1.5s

Type `time zygo exec demo '{"n": 2}'`
Enter
Sleep 1.5s

Type `# the same work, a fresh sandbox every time:`
Enter
Type `time zygo run --mount ./handler.py:/app/h.py:ro python:3.12-slim python3 -c 'import sys; sys.path.insert(0, "/app"); import h; print(h.handler({"n": 1}))'`
Enter
Sleep 2.5s

Type `zygo stop demo`
Enter
Sleep 1s
