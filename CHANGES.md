v0.31.0: Released 2026-07-22

- Some small changes across the board, linting, comment removal, etc.

---

v0.30.0: Released 2026-07-13

- Happy version 30! I am introducing a new CHANGES.md file to keep track of all the changes made in EAS_Listener. This will help maintain a clear history of updates and improvements made to EAS_Listener. EAS Tools uses the same kind of CHANGES.md file to keep track of changes made in EAS Tools. The CHANGES.md file will be updated with each new version, and it will include a summary of the changes made, along with the version number and date of the release.

- Introduce AGENTS.md file to help agentic development of EAS_Listener. This file will contain information about the project to help coding agents understand the project and its goals. It will include details about the architecture, design patterns, and coding standards used in EAS_Listener. The AGENTS.md file will be updated as needed to provide the most up-to-date information for coding agents working on EAS_Listener.

- Reduce Speechify Tom TTS dependency size to only the bare minimum required for the TTS engine to function. This will help reduce the overall size of the project and improve performance. (We also swapped over from Wine to spfy_synth, a NATIVE Linux binary that will run on Linux without Wine, which is a huge improvement for performance and stability. The UID match, however, is still 100% with the real Speechify Tom thanks to the in-line DLL FE loader.)

- Complete Icecast 2 stream integration. This was pending for the longest time, but never finished. Currently, this means that the listener can now output its own stream of alerts only, 24/7. There is a normal alert queue for frequent alert periods. The Icecast RELAY portion is 100% unmodified and works just the same.

- Add "Send Test Alert" button to the EAS_Listener GUI. This allows users to easily test the whole alert system pipeline without having to wait for an actual alert to occur. The test alert will simulate a real alert and will be sent through the same channels as a real alert, allowing users to verify that their setup is working correctly. Helpful if you recently changed something and don't know if your changes will work. The test alert will also be logged in the alert history for reference. Major thanks to GitHub user [@averlice](https://github.com/averlice) for the idea.

- Remove errant "icecast.xml" file from the Dockerfile. This file is not tracked locally and was causing build issues. Icecast should supply its own file.
