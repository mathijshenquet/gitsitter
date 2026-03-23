
The project is gitsitter, a git utility with the goal of making you forget the distinction between BRANCH and origin/BRANCH on your machine's.

It is a rust cli and deamon. The cli is used to control and configure the deamon. The deamon watches a set of git repo's in the background and fetches tracking remotes. Then it will ff merge both ways with the tracking branch as long as this is possible. 

## Configurability

scopes (hierachical override) 
- global (user)
- repo (path in fs)
- branches

per scope: 
- none (i.e. lack of key)
- fetch
- push
- pull
- push+pull

repo level: disabled (overrides all lower config and disables fetch/push/pull/push+pull)

the difference between fetch and none only exists on the repo level. And if any branch has fetch/push/pull/push+pull then it is implied that the repo has at least fetch.

default: push+pull


global:
refresh interval (global, repo)
audo add (default true)

## Technical details

Deamon: rust, include systemd service for user, toml config in central location. is a repo is disabled, do not

Cli: rust

There are post cd command hooks which automatically call `gitsitter register`. 

#### General control

`gitsitter status` or just `gitsitter` -> depending on location, shows:
- in git repo: info about current repo (is it tracked/when last fetched/exluded branches?) 
- not in git repo (or with `--global/-g`): show all tracked repo's

`gitsitter config` -> general config command, drops into TUI
`gitsitter config --repo/-r fetch`
`gitsitter config --branch/-b push+pull` etc 

`gitsitter disable/remove/rm` -> disable repo

`gitsitter enable/add` -> enable repo

#### Maintainence

`gitsitter register` -> adds a repo to gitsitter (with default/implied settings) generally not called by a human. Most other commands explicitly register before acting in the repo

`gitsitter install` -> install the deamon and the cd hooks, detect current shell (fish/bash/etc) but also allow options.


## General

Watch the registered git repo's (probably inside of .git somehow, what is the best way?) so that we can detect: (1) commits and push if necissary (2) react to deletion or move of location, baseline remove unless we can detect move cleanly

