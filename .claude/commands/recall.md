---
description: Search shared memory (Memnir) — keyword ranked + [[link]] expansion
allowed-tools: Bash(memnir search:*), Bash(memnir related:*), Read
---
Memnir memory search for: **$ARGUMENTS**

!`memnir search "$ARGUMENTS" --expand`

Using the ranked hits above (files live in `~/.claude/memnir/<file>`):

- If one memory is clearly the most relevant, **Read it** and answer "$ARGUMENTS" grounded in it — cite the file name.
- If several look relevant, give a one-line summary of the top 3 and ask which to open.
- The "related via [[links]]" section lists memories connected by links even without a keyword hit — surface them if they add context.

Be concise. Do not re-run the search; use the results above.