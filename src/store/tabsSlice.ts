import { createSlice, createAsyncThunk, PayloadAction } from '@reduxjs/toolkit';

export interface Tab {
  id: string;
  name: string;
  input: string;
  output: string;
  error: string;
  isProcessing: boolean;
}

interface TabsState {
  tabs: Tab[];
  activeTabId: string;
  tabCounter: number;
  activeTabContent: 'input' | 'output';
  isLoading: boolean;
  error: string | null;
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
  error: null
};

// Create Web Worker for JSON processing
const jsonWorker = new Worker(
  new URL('../workers/jsonWorker.ts', import.meta.url),
  { type: 'module' }
);

// Async thunks for JSON operations
export const processJsonData = createAsyncThunk(
  'tabs/processJson',
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
      jsonWorker.postMessage({ input, action });
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
      state.tabs = state.tabs.filter(tab => tab.id !== action.payload);
      if (state.activeTabId === action.payload) {
        state.activeTabId = state.tabs[0].id;
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
      }
    }
  },
  extraReducers: (builder) => {
    builder
      .addCase(processJsonData.pending, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.isProcessing = true;
          tab.error = '';
        }
      })
      .addCase(processJsonData.fulfilled, (state, action) => {
        const tab = state.tabs.find(t => t.id === action.meta.arg.tabId);
        if (tab) {
          tab.output = action.payload.output;
          tab.isProcessing = false;
          state.activeTabContent = 'output';
        }
      })
      .addCase(processJsonData.rejected, (state, action) => {
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
  clearTab
} = tabsSlice.actions;

export default tabsSlice.reducer;
