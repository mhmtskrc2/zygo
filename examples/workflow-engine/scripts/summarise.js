// The same shape in the other language the engine offers, so the worker has
// to choose an image per script rather than per platform.
//
// A `.js` entry means `runtime = "node"`, which means the Node agent: a pool
// of pre-loaded workers instead of a fork, and the same one-process-per-run
// guarantee. Nothing in this file knows that.
'use strict';

module.exports = function handler(event) {
  const items = event.items || [];
  const units = items.reduce((total, item) => total + Number(item.qty || 0), 0);
  return {
    id: event.id,
    line: `order ${event.id}: ${items.length} line(s), ${units} unit(s)`,
    skus: items.map((item) => String(item.sku)).sort(),
  };
};
