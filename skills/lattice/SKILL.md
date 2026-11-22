---
name: lattice
description: Find symbols, discover code by description, and check change impact in projects indexed by Lattice.
author: John Anthony Mariquit <john@mariquit.com>
license: Apache-2.0
metadata:
  copyright: Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>
  required-capabilities:
    - file-read
    - file-write
---

Run commands from the indexed project root. If `lattice` is unavailable or `.lattice/` is missing, use normal code navigation. Do not install models or initialize the index unless asked.

- Known symbol: `lattice query --symbol '<name>' --limit 5`
- Find code by description: `lattice query '<question>' --limit 5`
- Before editing: `lattice query --impact '<repo-relative path>' --limit 10`

Read only the relevant source spans. An empty result does not prove a symbol or caller is absent. Confirm important relationships in source. If a query fails or does not answer the question, use normal code navigation.
