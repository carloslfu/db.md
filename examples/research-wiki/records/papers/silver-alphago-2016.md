---
type: paper
id: silver-alphago-2016
created: 2026-04-28T16:10:00Z
updated: 2026-05-23T14:35:00Z
summary: "Silver et al.'s AlphaGo: the first program to beat a Go professional, combining supervised + RL policy networks, a value network, and MCTS — predecessor to AlphaZero"
title: "Mastering the game of Go with deep neural networks and tree search"
authors: [Silver, Huang, Maddison, Guez, Sifre, van den Driessche, Schrittwieser, Antonoglou, Panneershelvam, Lanctot, Dieleman, Grewe, Nham, Kalchbrenner, Sutskever, Lillicrap, Leach, Kavukcuoglu, Graepel, Hassabis]
year: 2016
venue: Nature
doi: "10.1038/nature16961"
url: https://www.nature.com/articles/nature16961
tags: [reinforcement-learning, self-play, mcts, go, deepmind]
source: [[sources/papers/silver-alphago-2016]]
concepts:
  - [[records/concepts/mcts]]
  - [[records/concepts/value-network]]
  - [[records/concepts/self-play]]
  - [[records/concepts/policy-gradient]]
---

# AlphaGo (Silver et al. 2016)

The first program to defeat a professional Go player on a full board. Its
recipe combined four parts: a policy network pretrained by supervised
learning on human expert moves, then refined by policy-gradient self-play;
a value network predicting the winner; and MCTS knitting the two together
at play time. AlphaZero ([[records/papers/silver-alphazero-2017]]) later
removed the human-data bootstrap, learning from self-play alone.

See [[records/concepts/mcts]] for the search, [[records/concepts/self-play]]
for the RL phase, and [[records/concepts/value-network]] for the evaluator.
