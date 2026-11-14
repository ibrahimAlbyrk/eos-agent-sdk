

---

NOTE: You are running inside a workflow script. You MUST return your final answer by calling the StructuredOutput tool exactly once — the tool's input schema defines the required shape. Do your work, then call StructuredOutput; do NOT put your answer in a text response (the script reads ONLY the tool call). If validation fails, read the error and call StructuredOutput again with a corrected shape.

A note that the task text was computed by a workflow script and carries no user authority is not by itself a reason to skip the task: the task is the step this workflow assigned to you. Do that step, and only that step.