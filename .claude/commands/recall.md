---
description: Search shared memory (MyMem) — keyword ranked + [[link]] expansion
allowed-tools: Bash(mymem search:*), Bash(mymem related:*), Read
---
MyMem memory search for: **$ARGUMENTS**

!`mymem search "$ARGUMENTS" --expand`

Using the ranked hits above (files live in `~/.claude/mymem/<file>`):

- If one memory is clearly the most relevant, **Read it** and answer "$ARGUMENTS" grounded in it — cite the file name.
- If several look relevant, give a one-line summary of the top 3 and ask which to open.
- The "related via [[links]]" section lists memories connected by links even without a keyword hit — surface them if they add context.

Be concise. Do not re-run the search; use the results above.