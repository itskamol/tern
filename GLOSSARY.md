# tern

An SSH terminal for macOS: the servers a person connects to, the live connections to them, and
how those are laid out on screen.

## Hosts and connections

**Host**:
A server the person can connect to, from a saved connection or a `~/.ssh/config` entry.
_Avoid_: server (in the UI), machine, target

**Saved connection**:
The settings tern keeps for one host in `hosts.json`: address, user, key, group, commands.
_Avoid_: connection (alone), profile, bookmark

**Session**:
One live login to one host, with its terminal.
_Avoid_: connection, shell (for the whole thing)

## Layout

**Tab**:
A part of the window holding one or more panes; one tab is in front at a time.
_Avoid_: window, workspace

**Active tab**:
The tab in front.
_Avoid_: current tab, focused tab

**Pane**:
The place in a tab where one session's terminal is shown.
_Avoid_: view, split (for the pane itself), frame

**Focused pane**:
The pane in a tab that the keyboard goes to.
_Avoid_: active pane, current pane

**Split**:
Dividing a pane in two, and the divider between the two panes that results.
_Avoid_: splitter, separator

**Main area**:
The region of the window right of the sidebar, showing the active tab or Settings.
_Avoid_: main pane

**Side panel**:
A panel beside the main area.
_Avoid_: right pane

**Files tab**:
A tab showing two file panels side by side, each the Mac's own files or one host's, for copying between them.
_Avoid_: SFTP browser, file manager

## At the prompt

**Command history**:
The command lines a person ran on a host, kept on this Mac only, with lines that look like secrets left out.
_Avoid_: shell history (that is the server's own `~/.bash_history`), log

**Fill**:
Typing a saved password into the remote's prompt, once, when the person presses the key for it.
_Avoid_: autofill, auto-type
