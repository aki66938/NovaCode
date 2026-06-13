import "@testing-library/jest-dom/vitest";

const createStorage = (): Storage => {
  let store = new Map<string, string>();
  return {
    get length() {
      return store.size;
    },
    clear: () => {
      store = new Map<string, string>();
    },
    getItem: (key: string) => store.get(key) ?? null,
    key: (index: number) => Array.from(store.keys())[index] ?? null,
    removeItem: (key: string) => {
      store.delete(key);
    },
    setItem: (key: string, value: string) => {
      store.set(key, String(value));
    }
  };
};

const storage = createStorage();

Object.defineProperty(globalThis, "localStorage", {
  configurable: true,
  value: storage
});

Object.defineProperty(window, "localStorage", {
  configurable: true,
  value: storage
});
