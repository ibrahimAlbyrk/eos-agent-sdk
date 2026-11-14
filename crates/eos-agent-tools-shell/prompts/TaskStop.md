
- Stops a running background task by its ID
- Takes a task_id parameter identifying the task to stop
- To stop a background agent, pass its agent ID or the name it was spawned with as task_id
- A subagent can stop only its own tasks: its background commands, monitors and workflow runs, and itself
- Returns a success or failure status
- Use this tool when you need to terminate a long-running task
