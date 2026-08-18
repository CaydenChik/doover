Output demo.mp4
Output demo.gif
Set Shell bash
Set FontSize 34
Set Width 1420
Set Height 780
Set Padding 40
Set TypingSpeed 55ms
Set Theme { "name": "doover", "background": "#17141F", "foreground": "#EDEAF6", "cursor": "#A78BFA", "black": "#17141F", "red": "#F87171", "green": "#6EE7B7", "yellow": "#FCD34D", "blue": "#A78BFA", "magenta": "#C4B5FD", "cyan": "#93C5FD", "white": "#EDEAF6", "brightBlack": "#6E6785", "brightRed": "#F87171", "brightGreen": "#6EE7B7", "brightYellow": "#FCD34D", "brightBlue": "#A78BFA", "brightMagenta": "#C4B5FD", "brightCyan": "#93C5FD", "brightWhite": "#FFFFFF" }
Hide
Type "source stage.sh && clear" Enter
Sleep 1s
Show
Type "ls photos/" Sleep 500ms Enter
Sleep 1.8s
Type "agent 'rm -rf dist/ photos/'   # your agent, tidying up" Sleep 500ms Enter
Sleep 1.8s
Type "ls photos/" Sleep 400ms Enter
Sleep 2s
Type "doover log" Sleep 400ms Enter
Sleep 2.4s
Type "doover undo" Sleep 400ms Enter
Sleep 2.6s
Type "ls photos/" Sleep 400ms Enter
Sleep 2s
Type "shasum -c sums.before   # byte for byte" Sleep 400ms Enter
Sleep 3.5s
