import {useEffect} from 'react';
import {Provider} from 'react-redux';
import {store} from './store';
import {loadTabsFromDB} from './store/tabsSlice';
import JsonViewer from './components/JsonViewer';

const App = () => {
  useEffect(() => {
    store.dispatch(loadTabsFromDB());
  }, []);
  return (
    <Provider store={store}>
      <JsonViewer/>
    </Provider>
  );
};

export default App;
