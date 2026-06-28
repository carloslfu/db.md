---
type: paper
id: williams-reinforce-1992
created: 2026-04-10T09:45:00Z
updated: 2026-05-23T14:10:00Z
summary: "Williams' REINFORCE: the class of likelihood-ratio policy-gradient estimators that optimize a parameterized policy directly by following the gradient of expected reward"
title: "Simple Statistical Gradient-Following Algorithms for Connectionist Reinforcement Learning"
authors: [Williams]
year: 1992
venue: Machine Learning
doi: "10.1007/BF00992696"
url: https://link.springer.com/article/10.1007/BF00992696
tags: [reinforcement-learning, policy-gradient, reinforce, foundations]
source: [[sources/papers/williams-reinforce-1992]]
concepts:
  - [[records/concepts/policy-gradient]]
  - [[records/concepts/actor-critic]]
---

# REINFORCE (Williams 1992)

Williams introduced the REINFORCE family of likelihood-ratio estimators,
the foundation of policy-gradient reinforcement learning. Rather than
learning a value function and deriving a policy, REINFORCE parameterizes
the policy directly and adjusts its weights along an unbiased Monte Carlo
estimate of the gradient of expected return. The estimator is high
variance, which later baseline and actor-critic methods address.

See [[records/concepts/policy-gradient]] for the derivation and
[[records/concepts/actor-critic]] for the variance-reduction line that
followed.
