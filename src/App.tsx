// src/App.tsx
import { Provider } from 'react-redux';
import { store } from './store';
import JsonViewer from './components/JsonViewer';

const App = () => {
  return (
    <Provider store={store}>
      <JsonViewer />
    </Provider>
  );
};

export default App;
