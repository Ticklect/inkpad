---
name: inkpad-notes
description: Use when a user asks to remember, note down, save, search, check, read, rename, pin, format, draw, export or organise something in their local Inkpad notebook (e.g. "note that down in Inkpad", "check my Inkpad notes", or "rename that page"). All operations are headless: never open the Inkpad window or use screen automation.
---

# Inkpad local notes — headless agent skill

When the user says **"note that down in Inkpad"** or asks to **check their notes**, use the Inkpad headless agent binary. It can run without the desktop window or a background web server.

## Command

Use an appropriate local command-execution tool:

`D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe`

The binary emits JSON to standard output. From PowerShell:

```powershell
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' list
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' search 'topic'
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' get NOTE_ID
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' create --title 'Topic' --body 'Actual facts and context to remember'
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' append NOTE_ID 'Additional information'
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' rename NOTE_ID 'New title'
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' pin NOTE_ID
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' font-size NOTE_ID 19
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' font-choice NOTE_ID Serif
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' active
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' activate NOTE_ID
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' export-text NOTE_ID 'D:\Notes\export.txt'
& 'D:\chat\inkpad\dist\Inkpad-Beta-Pending-Drag-Test\inkpad-agent.exe' export-png NOTE_ID 'D:\Notes\sketch.png'
```

If no command tool is available, connect the Inkpad MCP stdio server as documented in `D:\chat\inkpad\AGENTS.md`. Its 18 native MCP tools cover notes, pinned state, font choices, active selection, export, validated drawing strokes, undo, clear, and deletion without any GUI window. For full stroke JSON in Windows PowerShell 5.1, prefer `add-stroke NOTE_ID @D:\Notes\stroke.json` (use a string variable) or the numeric point syntax detailed in AGENTS.md.

## How to behave

- For **"note that down"**, summarise the context accurately, choose a concise descriptive title, and create or append an Inkpad note. Do not invent facts. Respond only after checking command success.
- For **"check my notes"**, run list or search and then get the relevant note; do not claim to have checked based on memory alone.
- Prefer appending to an existing exact note by ID rather than replacing its whole text.
- For rename, pin/unpin, font/size changes, active page selection, and exports, use the named action rather than editing the JSON file.
- Never manipulate files inside Inkpad's data folder directly; always use the headless CLI or MCP tools so concurrent changes are coordinated safely.
- **Never delete, clear drawings or undo a saved stroke** unless the user specifically requests it. These destructive actions require `--confirm` (CLI) or `confirm:true` (MCP).
- Treat the user's notes as sensitive. Do not paste them into public services or send them to a network endpoint.
- The Inkpad GUI need not be running. If it is open, saved external edits automatically appear in its sidebar after a short refresh.
- A page can contain text and drawing strokes simultaneously; the GUI's Text/Draw viewing mode is temporary and is not a remotely controlled persisted setting. Do not claim an agent changed that visual mode.
- If command execution fails, say so; do not claim the note was saved.

The notebook defaults to `%LOCALAPPDATA%\Inkpad\workspace.json` with an OS-level cross-process lock. For portable profiles, set `INKPAD_DATA_DIR` consistently.
