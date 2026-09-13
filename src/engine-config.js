(function exposeEngineConfig(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  root.XIXEngineConfig = api;
})(typeof globalThis === "object" ? globalThis : this, () => ({
  resolveSavedEngineConfig(engines, options) {
    if (!options || typeof options.engine !== "string") return null;
    if (options.engine === "video-colab-esrgan") {
      const engine = engines.find((candidate) => candidate.id === "video-colab");
      if (!engine) return null;
      const migrated = { ...options, engine: "video-colab" };
      migrated.scale = options.model === "realesrgan-x2plus" ? "2" : "4";
      migrated.interpolation = options.interpolation === "target"
        ? typeof options.target_fps === "string" ? options.target_fps : "off"
        : options.interpolation;
      delete migrated.model;
      delete migrated.target_fps;
      return { engine, options: migrated };
    }
    const engine = engines.find((candidate) => candidate.id === options.engine);
    return engine ? { engine, options } : null;
  },
}));
