import { createSHA256 } from "hash-wasm";

import { readFileChunks } from "./datasetUpload";

type HashRequest = { file: File };
type HashResponse = { type: "done"; hash: string } | { type: "error"; message: string };

self.onmessage = async (event: MessageEvent<HashRequest>) => {
  try {
    const hasher = await createSHA256();
    hasher.init();
    await readFileChunks(event.data.file, (chunk) => {
      hasher.update(chunk);
    });
    self.postMessage({ type: "done", hash: hasher.digest("hex") } satisfies HashResponse);
  } catch (error) {
    self.postMessage({
      type: "error",
      message: error instanceof Error ? error.message : "The file could not be hashed",
    } satisfies HashResponse);
  }
};
