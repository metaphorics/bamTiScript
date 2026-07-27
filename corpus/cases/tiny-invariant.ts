// Deterministic driver exercising observable behavior of tiny-invariant.
// Results are derived from the actual source, not hardcoded expectations.
//
// tiny-invariant captures `isProduction = process.env.NODE_ENV === 'production'`
// at module load time. We pin NODE_ENV before a single dynamic import so the
// driver is deterministic regardless of the ambient environment. The dev path
// is the richer surface: it exercises message formatting and lazy callback
// semantics. The production path only strips the message (a subset of dev).

type Result = {
  label: string;
  threw: boolean;
  message: string | null;
  lazyCalled: boolean;
};

const results: Result[] = [];

function capture(label: string, fn: () => void): void {
  let threw = false;
  let message: string | null = null;
  let lazyCalled = false;
  try {
    fn();
  } catch (e) {
    threw = true;
    message = e instanceof Error ? e.message : String(e);
  }
  results.push({ label, threw, message, lazyCalled });
}

function captureLazy(label: string, fn: (cb: () => string) => void): void {
  let threw = false;
  let message: string | null = null;
  let lazyCalled = false;
  try {
    fn(() => {
      lazyCalled = true;
      return 'lazy-msg';
    });
  } catch (e) {
    threw = true;
    message = e instanceof Error ? e.message : String(e);
  }
  results.push({ label, threw, message, lazyCalled });
}

// Pin NODE_ENV before import so isProduction is deterministic.
delete process.env.NODE_ENV;
const mod = await import('../projects/tiny-invariant/src/tiny-invariant.ts');
const invariant = mod.default;

// Truthy conditions: must NOT throw
const truthy: unknown[] = [1, -1, true, {}, [], 'hi', 1n];
for (let i = 0; i < truthy.length; i++) {
  capture(`truthy[${i}]`, () => invariant(truthy[i]));
}

// Falsy conditions: MUST throw with prefix
const falsy: unknown[] = [undefined, null, false, +0, -0, NaN, ''];
for (let i = 0; i < falsy.length; i++) {
  capture(`falsy[${i}]`, () => invariant(falsy[i]));
}

// Message provided as string
capture('falsy+string-msg', () => invariant(false, 'my message'));

// No message -> bare prefix
capture('falsy+no-msg', () => invariant(false));

// Lazy message: should NOT be called when condition is truthy
captureLazy('truthy+lazy', (cb) => invariant(true, cb));

// Lazy message: SHOULD be called when condition is falsy
captureLazy('falsy+lazy', (cb) => invariant(false, cb));

// Print stable, project-derived results
for (const r of results) {
  const parts = [r.label, r.threw ? 'THREW' : 'PASS'];
  if (r.threw) parts.push(`msg=${r.message}`);
  if (r.lazyCalled) parts.push('lazy=called');
  else if (r.label.includes('lazy')) parts.push('lazy=not-called');
  console.log(parts.join(' | '));
}
