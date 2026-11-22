// Representative PGO training workload for cz-js (QuickJS C core on Cortex-A53).
// Covers bytecode dispatch, loops, short-circuit comparisons, TypedArrays,
// plain objects, arrays, strings/UTF-8, RegExp, and JSON.parse/stringify.

function w_interp() {
  const prog = [
    1, 10, 1, 20, 2, 1, 3, 3, 4, 1, 5, 5, 1, 1, 2, 6, 1, 100, 7, 2,
    1, 7, 2, 1, 2, 4, 1, 1, 3, 8, 0
  ];
  let total = 0;
  for (let rep = 0; rep < 200; rep++) {
    const st = new Int32Array(32);
    let sp = 0, pc = 0, acc = rep;
    while (pc < prog.length) {
      const op = prog[pc++];
      if (op === 1) { st[sp++] = prog[pc++]; }
      else if (op === 2) { const b = st[--sp]; st[sp - 1] = (st[sp - 1] + b) | 0; }
      else if (op === 3) { const b = st[--sp]; st[sp - 1] = (st[sp - 1] - b) | 0; }
      else if (op === 4) { const b = st[--sp]; st[sp - 1] = (st[sp - 1] * b) | 0; }
      else if (op === 5) { const b = st[--sp]; st[sp - 1] = (st[sp - 1] ^ b) | 0; }
      else if (op === 6) { acc = (acc + st[sp - 1]) | 0; }
      else if (op === 7) { const lim = st[--sp]; if (acc < lim && sp > 0) pc = prog[pc]; else pc++; }
      else if (op === 8) { total = (total + st[--sp] + acc) | 0; }
      else break;
    }
  }
  return total;
}

function w_arith() {
  let x = 1.5, y = 2.5, z = 0.5, s = 0;
  for (let i = 0; i < 30000; i++) {
    x = x * 1.000001 + y * 0.000001;
    y = y * 0.999999 - z * 0.000001;
    z = (x - y) * 0.5;
    if (i & 1) s += z; else s -= z;
  }
  return s;
}

function lex(src) {
  let i = 0, n = src.length, toks = 0;
  while (i < n) {
    const c = src.charCodeAt(i);
    if (c === 32 || c === 9 || c === 10 || c === 13) { i++; continue; }
    if ((c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95) {
      i++;
      while (i < n) {
        const d = src.charCodeAt(i);
        if ((d >= 65 && d <= 90) || (d >= 97 && d <= 122) || (d >= 48 && d <= 57) || d === 95) i++;
        else break;
      }
      toks++;
    } else if (c >= 48 && c <= 57) {
      i++;
      while (i < n && src.charCodeAt(i) >= 48 && src.charCodeAt(i) <= 57) i++;
      toks++;
    } else {
      i++; toks++;
    }
  }
  return toks;
}

function w_lexer() {
  const chunk = 'function foo_bar(x1, y2) { if (x1 >= 10 && y2 <= 99) return x1 + y2 * 3; else return 0; }\n';
  let s = '';
  for (let k = 0; k < 20; k++) s += chunk;
  let t = 0;
  for (let r = 0; r < 40; r++) t += lex(s);
  return t;
}

function w_objects_json() {
  const items = [];
  for (let i = 0; i < 200; i++) {
    items.push({ id: i, name: 'item_' + i, score: i * 1.25, active: (i & 1) === 0, tags: ['a', 'b', 'c'] });
  }
  let acc = 0;
  for (let r = 0; r < 20; r++) {
    const s = JSON.stringify(items);
    const parsed = JSON.parse(s);
    for (let i = 0; i < parsed.length; i++) {
      acc += parsed[i].id + Object.keys(parsed[i]).length;
    }
  }
  const re = /^[a-z]+_\d+$/;
  let m = 0;
  for (let i = 0; i < 2000; i++) {
    if (re.test('item_' + i)) m++;
  }
  return acc + m;
}

function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2); }
function w_calls() {
  const obj = { add(a, b) { return a + b; } };
  let s = fib(24);
  for (let i = 0; i < 20000; i++) s += obj.add(i, 1);
  return s;
}

const r1 = w_interp();
const r2 = w_arith();
const r3 = w_lexer();
const r4 = w_objects_json();
const r5 = w_calls();
console.log('PGO_TRAIN_OK', r1, r2.toFixed(3), r3, r4, r5);
