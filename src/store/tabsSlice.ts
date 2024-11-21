import { createSlice, createAsyncThunk, PayloadAction } from '@reduxjs/toolkit';
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

const initialState: TabsState = {
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
};

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
  initialState,
  reducers: {
    addTab: (state) => {
      const newCounter = state.tabCounter + 1;
      const newTab: Tab = {
        id: `tab-${newCounter}`,
        name: `JSON ${newCounter}`,
        input: '',
        output: '',
        error: '',
        isProcessing: false
      };
      state.tabs.push(newTab);
      state.activeTabId = newTab.id;
      state.tabCounter = newCounter;
      state.activeTabContent = 'input';
    },

    removeTab: (state, action: PayloadAction<string>) => {
      if (state.tabs.length === 1) return;
      const newTabs = state.tabs.filter(tab => tab.id !== action.payload);
      state.tabs = newTabs;
      if (state.activeTabId === action.payload) {
        state.activeTabId = newTabs[0].id;
      }
    },

    updateTabInput: (state, action: PayloadAction<{ tabId: string; input: string }>) => {
      const tab = state.tabs.find(t => t.id === action.payload.tabId);
      if (tab) {
        tab.input = action.payload.input;
        tab.error = ''; // Clear any previous errors
      }
    },

    setActiveTab: (state, action: PayloadAction<string>) => {
      state.activeTabId = action.payload;
    },

    setActiveTabContent: (state, action: PayloadAction<'input' | 'output'>) => {
      state.activeTabContent = action.payload;
    },

    clearTab: (state, action: PayloadAction<string>) => {
      const tab = state.tabs.find(t => t.id === action.payload);
      if (tab) {
        tab.input = '';
        tab.output = '';
        tab.error = '';
        tab.isProcessing = false;
        tab.searchResults = undefined;
      }
    },

    setSearchQuery: (state, action: PayloadAction<string>) => {
      state.searchQuery = action.payload;
    },

    clearSearchResults: (state) => {
      state.tabs = state.tabs.map(tab => ({
        ...tab,
        searchResults: undefined
      }));
    }
  },
  extraReducers: (builder) => {
    builder
      .addCase(loadTabsFromDB.fulfilled, (state, action) => {
              if (action.payload?.length > 0) {
                state.tabs = action.payload;
                state.activeTabId = action.payload[0].id;
                state.tabCounter = Math.max(...action.payload.map(tab =>
                  parseInt(tab.id.split('-')[1] || '0')
                ));
              }
            })
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
      })
      .addCase(processJsonContent.pending, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.isProcessing = true;
          tab.error = '';
        }
      })
      .addCase(processJsonContent.fulfilled, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.output = action.payload.output;
          tab.isProcessing = false;
          state.activeTabContent = 'output';
        }
      })
      .addCase(processJsonContent.rejected, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.error = action.error.message || 'An error occurred processing the JSON';
          tab.isProcessing = false;
        }
      });
  }
});

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
