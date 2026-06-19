import * as fs from "node:fs";
import * as path from "node:path";

import { generateTransitionTree } from "./transitionTreeGenerator";

const DEFAULT_OUTPUT = path.resolve(
    __dirname,
    "..",
    "transitionTrees",
    "userConfigFields.json",
);

function main(): void {
    const outPath = path.resolve(process.argv[2] ?? DEFAULT_OUTPUT);
    const tree = generateTransitionTree();
    fs.mkdirSync(path.dirname(outPath), { recursive: true });
    fs.writeFileSync(outPath, JSON.stringify(tree, null, 2) + "\n", "utf8");
    process.stdout.write(`Wrote ${tree.fields.length} transition-tree field(s) to ${outPath}\n`);
}

if (require.main === module) {
    main();
}
