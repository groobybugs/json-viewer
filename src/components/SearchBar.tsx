import { useEffect, useState, useCallback } from 'react';
import { Search, X, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useAppDispatch, useAppSelector } from '../store';
import { searchInJson, setSearchQuery } from '../store/tabsSlice';
import debounce from 'lodash/debounce';

export const SearchBar = () => {
  const dispatch = useAppDispatch();
  const activeTabId = useAppSelector(state => state.tabs.activeTabId);
  const activeTab = useAppSelector(state =>
    state.tabs.tabs.find(tab => tab.id === activeTabId)
  );
  const searchQuery = useAppSelector(state => state.tabs.searchQuery);
  const [localQuery, setLocalQuery] = useState(searchQuery);

  // Debounced search function
  const debouncedSearch = useCallback(
    debounce((query: string) => {
      if (query.length >= 2 && activeTab?.input) {
        dispatch(searchInJson({
          tabId: activeTab.id,
          input: activeTab.input,
          query
        }));
      }
    }, 300),
    [activeTab]
  );

  useEffect(() => {
    return () => {
      debouncedSearch.cancel();
    };
  }, [debouncedSearch]);

  const handleSearch = (value: string) => {
    setLocalQuery(value);
    dispatch(setSearchQuery(value));
    debouncedSearch(value);
  };

  const clearSearch = () => {
    setLocalQuery('');
    dispatch(setSearchQuery(''));
  };

  const isSearching = activeTab?.isProcessing;

  return (
    <div className="relative flex items-center">
      <div className="relative flex-1">
        <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-slate-400" />
        <input
          type="text"
          value={localQuery}
          onChange={(e) => handleSearch(e.target.value)}
          placeholder="Search in JSON..."
          className="w-full h-9 pl-9 pr-8 rounded-md bg-slate-800 border border-slate-700
                   text-sm text-slate-200 placeholder-slate-400
                   focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent"
        />
        {localQuery && (
          <button
            onClick={clearSearch}
            className="absolute right-2 top-1/2 -translate-y-1/2 p-1 rounded-full
                     hover:bg-slate-700 text-slate-400 hover:text-slate-200"
          >
            {isSearching ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <X className="h-4 w-4" />
            )}
          </button>
        )}
      </div>
    </div>
  );
};

export const SearchResults = () => {
  const activeTabId = useAppSelector(state => state.tabs.activeTabId);
  const activeTab = useAppSelector(state =>
    state.tabs.tabs.find(tab => tab.id === activeTabId)
  );
  const searchQuery = useAppSelector(state => state.tabs.searchQuery);

  if (!searchQuery || !activeTab?.searchResults?.length) {
    return null;
  }

  return (
    <div className="mt-4 bg-slate-800 rounded-lg border border-slate-700">
      <div className="p-3 border-b border-slate-700">
        <h3 className="text-sm font-medium text-slate-200">
          Search Results ({activeTab.searchResults.length})
        </h3>
      </div>
      <div className="max-h-60 overflow-auto">
        {activeTab.searchResults.map((result, index) => (
          <div
            key={index}
            className="p-3 border-b border-slate-700 last:border-0 hover:bg-slate-700/50"
          >
            <div className="text-sm text-slate-400 font-mono">
              {result.path.join(' → ')}
            </div>
            <div className="mt-1 text-sm text-slate-200">
              {typeof result.value === 'object'
                ? JSON.stringify(result.value)
                : String(result.value)
              }
            </div>
          </div>
        ))}
      </div>
    </div>
  );
};
