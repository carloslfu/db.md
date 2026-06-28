---
type: paper
id: silver-alphazero-2017
created: 2026-05-20T15:05:00Z
updated: 2026-05-23T16:30:00Z
summary: "DeepMind's AlphaZero: one self-play + MCTS + neural-net recipe reaching superhuman chess, shogi, and Go with no domain knowledge beyond the rules"
title: "Mastering Chess and Shogi by Self-Play with a General Reinforcement Learning Algorithm"
authors: [Silver, Hubert, Schrittwieser, Antonoglou, Lai, Guez, Lanctot, Sifre, Kumaran, Graepel, Lillicrap, Simonyan, Hassabis]
year: 2017
venue: arXiv
arxiv_id: "1712.01815"
url: https://arxiv.org/abs/1712.01815
tags: [reinforcement-learning, self-play, mcts, deepmind]
source: [[sources/papers/silver-alphazero-2017]]
concepts:
  - [[records/concepts/self-play]]
  - [[records/concepts/mcts]]
  - [[records/concepts/policy-iteration]]
  - [[records/concepts/value-network]]
  - [[records/concepts/function-approximation]]
---

# AlphaZero

DeepMind's generalization of AlphaGo Zero from Go to chess and
shogi using the same self-play + MCTS + neural-net recipe with no
domain knowledge beyond rules. Reached superhuman strength in all
three games within 24 hours. Where the original AlphaGo
([[records/papers/silver-alphago-2016]]) bootstrapped from human
expert games, its successor AlphaGo Zero dropped that crutch for Go;
AlphaZero generalizes the same human-free,
[[records/concepts/self-play]]-only recipe to chess and shogi as well.

See [[records/concepts/self-play]] for the technique, [[records/concepts/mcts]]
for the search component, and the lineage page
[[records/synthesis/deep-rl-lineage]] for where it sits in the arc.
