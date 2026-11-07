# Agent

This is a basic coding agent, for me, by me. The goal is to have minimal dependencies, and only implement my most used features of other coding agents. The code in this repo is small enough you can comfortably read all of it.

Assistant answers are rendered as Markdown in styled terminals (bold, italics,
tables, and code blocks). Answers are buffered until the response ends; reasoning
still streams immediately. Redirected output and `NO_COLOR` preserve raw Markdown.
Code blocks are formatted without language-specific syntax highlighting.

Feature Roadmap to add:
- good subagent use/orchestration
- better support of prompt edits/revisions/forking
- swarm support (agent-agent comms)
- self-scaffolding/RLM esque features
- seamless syncing to cloud agent and back
- smarter permissions management and sandboxing
