// src/store/tabsSlice.ts
import { createSlice, createAsyncThunk } from '@reduxjs/toolkit';
import { openDB } from 'idb';

// Initialize IndexedDB
const initDB = async () => {
  const db = await openDB('jsonViewerDB', 1, {
    upgrade(db) {
      db.createObjectStore('tabs');
      db.createObjectStore('files');
    },
  });
  return db;
};

export interface Tab {
  id: string;
  name: string;
  input: string;
  output: string;
  error: string;
  isProcessing: boolean;
  searchResults?: Array<{ path: string[]; value: any }>;
}

interface TabsState {
  tabs: Tab[];
  activeTabId: string;
  tabCounter: number;
  activeTabContent: 'input' | 'output';
  isLoading: boolean;
  error: string | null;
  searchQuery: string;
}

// Worker instance
const jsonWorker = new Worker(
  new URL('../workers/jsonWorker.ts', import.meta.url),
  { type: 'module' }
);

// Async thunks
export const loadTabsFromDB = createAsyncThunk(
  'tabs/loadFromDB',
  async () => {
    const db = await initDB();
    const tabs = await db.getAll('tabs');
    return tabs;
  }
);

export const saveTabToDB = createAsyncThunk(
  'tabs/saveToDB',
  async (tab: Tab) => {
    const db = await initDB();
    await db.put('tabs', tab, tab.id);
    return tab;
  }
);

export const processJsonContent = createAsyncThunk(
  'tabs/processContent',
  async ({ tabId, input, action }: { tabId: string; input: string; action: 'format' | 'minify' }) => {
    return new Promise<{ output: string }>((resolve, reject) => {
      const messageHandler = (event: MessageEvent) => {
        if (event.data.error) {
          reject(event.data.error);
        } else {
          resolve({ output: event.data.result });
        }
        jsonWorker.removeEventListener('message', messageHandler);
      };

      jsonWorker.addEventListener('message', messageHandler);
      jsonWorker.postMessage({ type: 'process', input, action });
    });
  }
);

export const searchInJson = createAsyncThunk(
  'tabs/search',
  async ({ tabId, input, query }: { tabId: string; input: string; query: string }) => {
    return new Promise<{ results: Array<{ path: string[]; value: any }> }>((resolve, reject) => {
      const messageHandler = (event: MessageEvent) => {
        if (event.data.error) {
          reject(event.data.error);
        } else {
          resolve({ results: event.data.results });
        }
        jsonWorker.removeEventListener('message', messageHandler);
      };

      jsonWorker.addEventListener('message', messageHandler);
      jsonWorker.postMessage({ type: 'search', input, searchQuery: query });
    });
  }
);

const tabsSlice = createSlice({
  name: 'tabs',
  initialState: {
    tabs: [{
      id: 'tab-1',
      name: 'JSON 1',
      input: '',
      output: '',
      error: '',
      isProcessing: false
    }],
    activeTabId: 'tab-1',
    tabCounter: 1,
    activeTabContent: 'input',
    isLoading: false,
    error: null,
    searchQuery: ''
  } as TabsState,
  reducers: {
    // ... previous reducers ...

    setSearchQuery(state, action) {
      state.searchQuery = action.payload;
    },

    clearSearchResults(state) {
      state.tabs = state.tabs.map(tab => ({
        ...tab,
        searchResults: undefined
      }));
    }
  },
  extraReducers: (builder) => {
    // ... previous extra reducers ...

    builder
      .addCase(searchInJson.pending, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.isProcessing = true;
        }
      })
      .addCase(searchInJson.fulfilled, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.searchResults = action.payload.results;
          tab.isProcessing = false;
        }
      })
      .addCase(searchInJson.rejected, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.error = action.error.message || 'Search failed';
          tab.isProcessing = false;
        }
      });
  }
});

clearTab: (state, action: PayloadAction<string>) => {
  const tab = state.tabs.find(t => t.id === action.payload);
  if (tab) {
    tab.input = '';
    tab.output = '';
    tab.error = '';
    tab.isProcessing = false;
    tab.searchResults = undefined;
  }
}

export const {
    addTab,
    removeTab,
    updateTabInput,
    setActiveTab,
    setActiveTabContent,
    clearTab,
    setSearchQuery,
    clearSearchResults
} = tabsSlice.actions;

export default tabsSlice.reducer;
