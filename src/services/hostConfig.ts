import type { AppConfig } from './api';

interface ConfigStore {
  setConfig(config: AppConfig): Promise<void>;
  getConfig(): Promise<AppConfig>;
}

/** Serialize completed edits without blocking the next UI action or replacing newer drafts. */
export function createHostConfigSaver(config: AppConfig, store: ConfigStore) {
  let pending = Promise.resolve();
  const saveConfig = () => {
    const snapshot: AppConfig = JSON.parse(JSON.stringify(config));
    const save = pending
      .catch(() => undefined)
      .then(async () => {
        await store.setConfig(snapshot);
        const saved = await store.getConfig();
        for (const key of Object.keys(saved) as (keyof AppConfig)[]) {
          if (JSON.stringify(config[key]) === JSON.stringify(snapshot[key]))
            Object.assign(config, { [key]: saved[key] });
        }
      });
    pending = save;
    return save;
  };
  return Object.assign(saveConfig, { flush: () => pending });
}
