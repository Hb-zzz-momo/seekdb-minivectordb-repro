# MiniVectorDB — pinned public reproduction

Database source: [zhangxh0526/seekdb](https://github.com/zhangxh0526/seekdb/tree/4d575f663c5e408987b441c53ece9c0189860908), commit `4d575f663c5e408987b441c53ece9c0189860908`.

The database implementation is third-party public code, not newly authored by the current participant. All 30 source/build files, including Dockerfile and the Rust toolchain pin, retain the original bytes. `agent/source-baseline-manifest.json` records their SHA-256 fingerprints.

`agent/conversation.json` contains the current participant's authentic requests, Agent explanations and actual tool-result summaries, with roles and limitations. The original author's learning logs are not used as the current participant's experience. The historical author logs remain available through the original source commit. Agent explanations, automatic tests and local reviews do not establish student understanding or an official score.

## Build and public preflight

```bash
docker build -t minivectordb:repro .
python3 protocol_check/run.py --image minivectordb:repro --handshake-only --output handshake-results
python3 protocol_check/run.py --image minivectordb:repro --output public-results
```

Server111 native checks used Rust 1.96.0: build passed; 75 tests passed, 0 failed and 3 benchmark tests remained ignored; all 20 public protocol cases passed. Separately, the unchanged pinned Dockerfile built successfully on server111; its default ENTRYPOINT passed the official checker handshake and all 20 public cases under the checker container restrictions. Image: `sha256:ab34e552f3c307a85c3b2b4f1f59997f93820cb281e8a1593ea037d5c2a176d0`. Local checks do not establish hidden-case correctness, full crash-recovery qualification or platform acceptance.

No official submission, performance score or Prompt score is claimed. Use this repository's final `2026-competition-round2` commit SHA, not the historical source SHA, when submitting these learning materials.
