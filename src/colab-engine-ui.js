(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  root.XIXColabEngineUI = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  function resolveStartRequirement(engine, hasInput, hasOutput) {
    if (engine && (engine.id === "video-colab" || engine.remote)) {
      return hasInput && hasOutput ? "colab-remote" : "input-output-required";
    }
    return hasInput && hasOutput ? "local-batch" : "input-output-required";
  }

  function splitEngineLabel(name) {
    const text = String(name || "");
    const match = /^(.*?)\s+(\((Online|Offline|Colab)\))$/.exec(text);
    return {
      label: match ? match[1] : text,
      mode: match ? match[2] : "",
    };
  }

  function filterCompatibleFiles(files, extensions) {
    const accepted = new Set((extensions || []).map((ext) => String(ext).toLowerCase()));
    return (files || []).filter((file) => {
      const path = String(file && (file.path || file.name || file));
      const dot = path.lastIndexOf(".");
      return dot >= 0 && accepted.has(path.slice(dot + 1).toLowerCase());
    });
  }

  function resolveMuteControl(engineId, pressed) {
    const visible = engineId === "video-colab";
    return { visible, pressed: visible && !!pressed };
  }

  function collectAdvancedOptions(fields) {
    const options = {};
    for (const [id, field] of Object.entries(fields || {})) options[id] = field.read();
    return options;
  }

  function createAdvancedOptionSession() {
    const valuesByEngine = new Map();
    return {
      capture(engineId, fields) {
        if (engineId) valuesByEngine.set(engineId, collectAdvancedOptions(fields));
      },
      restore(engineId, fields) {
        const values = valuesByEngine.get(engineId);
        if (!values) return false;
        for (const [id, value] of Object.entries(values)) {
          if (fields[id]) fields[id].set(value);
        }
        return true;
      },
    };
  }

  function createMuteControl({ button, getEngineId, onChange }) {
    let pressed = false;
    let phase = "idle";

    function sync() {
      const state = resolveMuteControl(getEngineId(), pressed);
      pressed = state.pressed;
      button.classList.toggle("hidden", !state.visible);
      button.classList.toggle("on", pressed);
      button.setAttribute("aria-pressed", String(pressed));
      const locked = phase === "running" || phase === "paused";
      button.disabled = locked;
      button.setAttribute("aria-disabled", String(locked));
    }

    const control = {
      el: button,
      read: () => pressed,
      set(value) {
        pressed = !!value;
        sync();
      },
      setPhase(value) {
        phase = value;
        sync();
      },
      toggle() {
        if (button.disabled || button.classList.contains("hidden")) return false;
        control.set(!pressed);
        if (onChange) onChange(pressed);
        return true;
      },
    };
    return control;
  }

  return {
    resolveStartRequirement,
    splitEngineLabel,
    filterCompatibleFiles,
    resolveMuteControl,
    collectAdvancedOptions,
    createAdvancedOptionSession,
    createMuteControl,
  };
});
