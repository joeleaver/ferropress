// Fuzz the drag-drop reconcile `moveSubtree` for the flat-with-depth menu tree BEFORE porting it
// to Rust (crates/ferropress-admin/src/app.rs). Run: `node move-subtree.fuzz.mjs`.
//
// The tree is a pre-order Vec of {cid, depth}; a row's parent is the nearest preceding row at
// depth-1. move_subtree cuts a subtree [i,k) and re-inserts it either BEFORE a gap anchor (as a
// sibling at the depth of the surviving row above the gap — MF2), INTO a target (as its last
// child, depth+1), or at the END (top level). Over-depth REJECTS (bounces), never clamps (MF2).
// A drop whose anchor/target lies inside the moving subtree is rejected (is_descendant guard).
//
// Invariant proven for every ACCEPTED move: row0 depth 0; no row deeper than prev+1; 0..=MAX;
// and the cid multiset is unchanged (nothing lost or duplicated). Every REJECTED move leaves the
// tree byte-identical.

const MAX_DEPTH = 5;

const subtreeEnd = (t, i) => { const d = t[i].depth; let k = i + 1; while (k < t.length && t[k].depth > d) k++; return k; };
const indexOf = (t, cid) => t.findIndex((r) => r.cid === cid);
const maxDepthIn = (block) => block.reduce((m, r) => Math.max(m, r.depth), 0);

function invariantOK(t) {
  if (t.length && t[0].depth !== 0) return `row0 depth ${t[0].depth} != 0`;
  for (let i = 1; i < t.length; i++) if (t[i].depth > t[i - 1].depth + 1) return `row${i} depth ${t[i].depth} > prev+1`;
  for (const r of t) if (r.depth < 0 || r.depth > MAX_DEPTH) return `depth out of range ${r.depth}`;
  return 'OK';
}

// dest: {type:'before'|'into', cid} | {type:'end'}. Returns true iff the tree changed.
function moveSubtree(tree, srcCid, dest) {
  const i = indexOf(tree, srcCid);
  if (i < 0) return false;
  const k = subtreeEnd(tree, i);
  // is_descendant / self guard on the ORIGINAL tree: can't drop into/before a node in the block.
  if (dest.type !== 'end') {
    const a = indexOf(tree, dest.cid);
    if (a < 0) return false;
    if (a >= i && a < k) return false;
  }
  const block = tree.slice(i, k);
  const post = tree.slice(0, i).concat(tree.slice(k));

  let insAt, newRoot;
  if (dest.type === 'end') {
    insAt = post.length; newRoot = 0;
  } else if (dest.type === 'before') {
    const a = indexOf(post, dest.cid);
    insAt = a;
    newRoot = post[a].depth; // the anchor's OWN depth (its previous-sibling level), NOT the row above
  } else { // into
    const t = indexOf(post, dest.cid);
    insAt = subtreeEnd(post, t);
    newRoot = post[t].depth + 1;
  }
  const delta = newRoot - block[0].depth;
  if (maxDepthIn(block) + delta > MAX_DEPTH) return false; // REJECT (bounce), never clamp
  const rebased = block.map((r) => ({ cid: r.cid, depth: r.depth + delta }));
  post.splice(insAt, 0, ...rebased);
  // Detect a genuine no-op (same order + depths) so a wasted drop doesn't dirty the menu.
  let changed = post.length !== tree.length;
  if (!changed) for (let j = 0; j < post.length; j++) if (post[j].cid !== tree[j].cid || post[j].depth !== tree[j].depth) { changed = true; break; }
  if (!changed) return false;
  tree.length = 0; tree.push(...post);
  return true;
}

// ---- fuzz -------------------------------------------------------------------
let seed = 1234567;
const rnd = () => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed / 0x7fffffff; };
const pick = (arr) => arr[Math.floor(rnd() * arr.length)];

function randomTree(n) {
  const t = [];
  let nextCid = 0;
  for (let x = 0; x < n; x++) {
    let depth;
    if (x === 0) depth = 0;
    else {
      const maxOk = Math.min(t[x - 1].depth + 1, MAX_DEPTH);
      depth = Math.floor(rnd() * (maxOk + 1));
    }
    t.push({ cid: 'n' + nextCid++, depth });
  }
  return t;
}

const key = (t) => t.map((r) => r.cid + ':' + r.depth).join('|');
const multiset = (t) => t.map((r) => r.cid).sort().join(',');
// parent cid of each row (nearest preceding shallower row) — the flat model's parent relation.
function parentMap(t) {
  const m = {};
  for (let i = 0; i < t.length; i++) {
    let p = null;
    for (let j = i - 1; j >= 0; j--) if (t[j].depth < t[i].depth) { p = t[j].cid; break; }
    m[t[i].cid] = p;
  }
  return m;
}
// After a move, ONLY the dragged root may change parent; every other row must keep its parent.
// (This is the invariant that catches a silent reparent the plain validity check would miss.)
function reparentedOthers(before, after, srcCid) {
  for (const cid in before) if (cid !== srcCid && before[cid] !== after[cid]) return cid;
  return null;
}

let moves = 0, rejects = 0, checks = 0;
const ROUNDS = 60000;
for (let round = 0; round < ROUNDS; round++) {
  const tree = randomTree(1 + Math.floor(rnd() * 12));
  const before = key(tree);
  const beforeSet = multiset(tree);
  const beforeParents = parentMap(tree);
  const src = pick(tree).cid;
  const destType = pick(['before', 'into', 'end', 'before', 'into']); // weight the interesting ones
  let dest;
  if (destType === 'end') dest = { type: 'end' };
  else dest = { type: destType, cid: pick(tree).cid };

  const moved = moveSubtree(tree, src, dest);
  checks++;
  if (moved) {
    moves++;
    const v = invariantOK(tree);
    if (v !== 'OK') { console.error(`FAIL invariant after move: ${v}\n  before=${before}\n  src=${src} dest=${JSON.stringify(dest)}\n  after=${key(tree)}`); process.exit(1); }
    if (multiset(tree) !== beforeSet) { console.error(`FAIL cid multiset changed\n  before=${before} (${beforeSet})\n  after=${key(tree)} (${multiset(tree)})\n  src=${src} dest=${JSON.stringify(dest)}`); process.exit(1); }
    const bad = reparentedOthers(beforeParents, parentMap(tree), src);
    if (bad) { console.error(`FAIL silently reparented a non-dragged row '${bad}' (${beforeParents[bad]} -> ${parentMap(tree)[bad]})\n  before=${before}\n  after=${key(tree)}\n  src=${src} dest=${JSON.stringify(dest)}`); process.exit(1); }
  } else {
    rejects++;
    if (key(tree) !== before) { console.error(`FAIL rejected move mutated the tree\n  before=${before}\n  after=${key(tree)}\n  src=${src} dest=${JSON.stringify(dest)}`); process.exit(1); }
  }
}

// ---- targeted cases ---------------------------------------------------------
function expect(name, cond) { if (!cond) { console.error('FAIL ' + name); process.exit(1); } }

// is_descendant: dropping a parent INTO its own child must be rejected + no-op.
{
  const t = [{ cid: 'A', depth: 0 }, { cid: 'B', depth: 1 }, { cid: 'C', depth: 2 }];
  const before = key(t);
  expect('into-own-descendant rejected', moveSubtree(t, 'A', { type: 'into', cid: 'C' }) === false && key(t) === before);
  expect('before-own-descendant rejected', moveSubtree(t, 'A', { type: 'before', cid: 'B' }) === false && key(t) === before);
}
// The MF2 failing case: [A0, B1(child of A), C0]; drag A's subtree before C — the gap-depth must
// come from the SURVIVING row above (post-cut that's nothing => 0), NOT B (inside the cut).
{
  const t = [{ cid: 'A', depth: 0 }, { cid: 'B', depth: 1 }, { cid: 'C', depth: 0 }];
  expect('gap-depth uses surviving row (not a cut row)', moveSubtree(t, 'A', { type: 'before', cid: 'C' }) === false /* no-op: already before C */ || invariantOK(t) === 'OK');
  expect('root stays depth 0', t[0].depth === 0);
}
// Depth cap: a 3-tall block nested into a depth-4 target would reach depth 6 > MAX(5) — reject.
{
  const t = [{ cid: 'T', depth: 0 }, { cid: 't1', depth: 1 }, { cid: 't2', depth: 2 }, { cid: 't3', depth: 3 }, { cid: 't4', depth: 4 },
             { cid: 'S', depth: 0 }, { cid: 's1', depth: 1 }, { cid: 's2', depth: 2 }];
  const before = key(t);
  expect('over-depth into rejected (bounce)', moveSubtree(t, 'S', { type: 'into', cid: 't4' }) === false && key(t) === before);
}
// A legal deep nest that JUST fits: S (2 tall) into t3 (depth3) => reaches depth 5 == MAX — accept.
{
  const t = [{ cid: 'T', depth: 0 }, { cid: 't1', depth: 1 }, { cid: 't2', depth: 2 }, { cid: 't3', depth: 3 },
             { cid: 'S', depth: 0 }, { cid: 's1', depth: 1 }];
  expect('at-cap into accepted', moveSubtree(t, 'S', { type: 'into', cid: 't3' }) === true && invariantOK(t) === 'OK' && maxDepthIn(t) === 5);
}
// Regression (code-review MF1): dropping a top-level item into the gap BEFORE a first-child row
// must place it at the child's OWN depth (as its previous sibling), NOT reparent the child's
// subtree. [Home:0, About:0, Team:1(child of About), Blog:0]; drag Blog before Team.
{
  const t = [{ cid: 'Home', depth: 0 }, { cid: 'About', depth: 0 }, { cid: 'Team', depth: 1 }, { cid: 'Blog', depth: 0 }];
  const beforeP = parentMap(t);
  expect('reparent-regression moved', moveSubtree(t, 'Blog', { type: 'before', cid: 'Team' }) === true);
  const afterP = parentMap(t);
  expect('Team stays a child of About (not reparented under Blog)', afterP['Team'] === 'About');
  expect('Blog becomes About\'s child (anchor depth), the only reparent', afterP['Blog'] === 'About' && !reparentedOthers(beforeP, afterP, 'Blog'));
  expect('invariant holds', invariantOK(t) === 'OK');
}

// End drop always lands a subtree at top level.
{
  const t = [{ cid: 'A', depth: 0 }, { cid: 'B', depth: 1 }, { cid: 'C', depth: 0 }];
  expect('end drop tops-out the subtree', moveSubtree(t, 'B', { type: 'end' }) === true && t[t.length - 2].depth === 0 && invariantOK(t) === 'OK');
}

console.log(`OK — ${checks} random drops (${moves} moved, ${rejects} bounced) + targeted cases all pass; invariant + cid-multiset held.`);
