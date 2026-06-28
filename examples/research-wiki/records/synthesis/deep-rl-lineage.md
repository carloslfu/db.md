---
type: synthesis
meta-type: conclusion
id: deep-rl-lineage
created: 2026-05-23T16:00:00Z
updated: 2026-05-23T16:00:00Z
summary: "The arc from TD-Gammon to AlphaZero: how temporal-difference learning, deep function approximation, policy gradients, and tree search compounded into modern deep RL"
topic: deep-rl-lineage
tags: [reinforcement-learning, history, synthesis, deep-rl]
derived_from:
  - [[records/papers/sutton-td-1988]]
  - [[records/papers/tesauro-tdgammon-1995]]
  - [[records/papers/watkins-qlearning-1992]]
  - [[records/papers/williams-reinforce-1992]]
  - [[records/papers/mnih-dqn-2015]]
  - [[records/papers/mnih-a3c-2016]]
  - [[records/papers/schulman-trpo-2015]]
  - [[records/papers/schulman-ppo-2017]]
  - [[records/papers/silver-alphago-2016]]
  - [[records/papers/silver-alphazero-2017]]
  - [[records/concepts/temporal-difference-learning]]
  - [[records/concepts/q-learning]]
  - [[records/concepts/policy-gradient]]
  - [[records/concepts/actor-critic]]
  - [[records/concepts/function-approximation]]
  - [[records/concepts/experience-replay]]
  - [[records/concepts/trust-region-methods]]
  - [[records/concepts/self-play]]
  - [[records/concepts/mcts]]
---

# The deep-RL lineage

This page traces the through-line that the per-concept records establish
locally: how a handful of ideas — bootstrapped prediction, deep function
approximation, direct policy optimization, and learned tree search —
compounded over three decades into modern deep reinforcement learning. It
is a synthesis across the wiki's concepts and papers, not new analysis; each
claim is anchored in a record that argues it in full. Every method here is a
way of solving or approximating a [[records/concepts/markov-decision-process]].

## 1. Bootstrapping (1988) and the first proof of concept (1995)

The lineage starts with prediction. Sutton's
[[records/papers/sutton-td-1988]] formalized
[[records/concepts/temporal-difference-learning]]: learn a value estimate
from a *later* estimate rather than from the final outcome. Seven years
later Tesauro's [[records/papers/tesauro-tdgammon-1995]] turned the theory
into a headline result — a backgammon [[records/concepts/value-network]]
trained by TD over [[records/concepts/self-play]] reaching world-class
strength. TD-Gammon was also the first prominent case of nonlinear
[[records/concepts/function-approximation]] working in RL, years ahead of
the theory.

## 2. Control: value and policy, two branches (1992)

Two 1992 papers opened the two branches RL has followed ever since. Watkins
& Dayan's [[records/papers/watkins-qlearning-1992]] gave the **value**
branch its workhorse: [[records/concepts/q-learning]], off-policy TD control
with a convergence proof. Williams' [[records/papers/williams-reinforce-1992]]
gave the **policy** branch its foundation: REINFORCE, the likelihood-ratio
[[records/concepts/policy-gradient]] that optimizes a policy directly. Both
branches must still confront [[records/concepts/exploration-exploitation]] —
the value branch through ε-greedy behaviour, the policy branch through
entropy and stochasticity.

## 3. The deep turn (2015–2016)

The deep era arrived when both branches were scaled with neural networks.
On the value side, DQN ([[records/papers/mnih-dqn-2015]]) combined
Q-learning with a deep network on Atari pixels and made the unstable mixture
work with [[records/concepts/experience-replay]] and a frozen target
network — the canonical management of the **deadly triad** that
[[records/concepts/function-approximation]] introduces. On the policy side,
A3C ([[records/papers/mnih-a3c-2016]]) scaled [[records/concepts/actor-critic]]
by replacing the replay buffer with parallel actors, decorrelating
on-policy data instead of reusing off-policy data.

## 4. Stable policy optimization (2015–2017)

Policy gradients are fragile to step size, so the next move was to bound the
update. [[records/concepts/trust-region-methods]] answered this twice: TRPO
([[records/papers/schulman-trpo-2015]]) with a hard KL trust region and a
monotonic-improvement guarantee, then PPO
([[records/papers/schulman-ppo-2017]]) with a cheap clipped objective that
captured most of the benefit with first-order optimization. PPO became the
default — and, later, the workhorse of RLHF.

## 5. Search closes the loop (2016–2017)

The final strand folds search back in. AlphaGo
([[records/papers/silver-alphago-2016]]) combined a policy-gradient-refined
policy network, a value network, and [[records/concepts/mcts]] to beat a Go
professional. AlphaGo Zero then dropped the human-game bootstrap for Go,
and AlphaZero ([[records/papers/silver-alphazero-2017]]) generalized that
human-free recipe to chess and shogi: pure [[records/concepts/self-play]]
drives a [[records/concepts/policy-iteration]] loop in which MCTS is the
improvement operator and a shared value/policy network is the evaluator. The
recipe generalized across chess, shogi, and Go with no domain knowledge
beyond the rules — closing the arc that TD-Gammon opened, now with deep
networks and learned search.

## The shape of the arc

Read end to end, the lineage is a story of **compounding primitives**.
Bootstrapping made prediction online; function approximation made it
generalize; the value and policy branches gave two ways to turn prediction
into control; deep networks scaled both; trust regions made the policy
branch stable; and tree search plus self-play recombined the pieces into
systems that learn superhuman play from nothing but the rules. No single
idea is the breakthrough — the breakthrough is the stack.

## Anchored records

Each stage above is argued in full on its own page; this synthesis only
sequences them. For the mechanisms, follow the links into
[[records/concepts/temporal-difference-learning]],
[[records/concepts/q-learning]], [[records/concepts/policy-gradient]],
[[records/concepts/actor-critic]], [[records/concepts/function-approximation]],
[[records/concepts/experience-replay]],
[[records/concepts/trust-region-methods]], [[records/concepts/self-play]],
and [[records/concepts/mcts]].
